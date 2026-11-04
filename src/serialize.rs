//! Serialize a database.
use std::ffi::CStr;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

use crate::error::{error_from_handle, error_from_sqlite_code};
use crate::ffi;
use crate::{Connection, Error, Name, Result};

// SQLite recommends 20 bytes of padding after untrusted serialized input.
const DESERIALIZE_PADDING: usize = 20;

/// Shared (`SQLITE_SERIALIZE_NOCOPY`) serialized database
pub struct SharedData<'conn> {
    phantom: PhantomData<&'conn mut Connection>,
    ptr: NonNull<u8>,
    sz: usize,
}

/// Owned serialized database
pub struct OwnedData {
    ptr: NonNull<u8>,
    sz: usize,
    capacity: usize,
}

impl OwnedData {
    /// # Safety
    ///
    /// `ptr` must uniquely own an allocation from `sqlite3_malloc64` containing
    /// at least `sz` initialized bytes. `sz` must not exceed `isize::MAX`.
    #[must_use]
    pub unsafe fn from_raw_nonnull(ptr: NonNull<u8>, sz: usize) -> Self {
        Self {
            ptr,
            sz,
            capacity: sz,
        }
    }

    fn zeroed(sz: usize) -> Result<Self> {
        let capacity = padded_size(sz)?;
        let ptr = NonNull::new(unsafe { ffi::sqlite3_malloc64(capacity as u64) }.cast::<u8>())
            .ok_or_else(|| error_from_sqlite_code(ffi::SQLITE_NOMEM, None))?;

        // The fresh allocation contains capacity bytes, initialized before any Rust slice is formed.
        unsafe { ptr.as_ptr().write_bytes(0, capacity) };
        Ok(Self { ptr, sz, capacity })
    }

    fn ensure_padding(&mut self) -> Result<()> {
        let capacity = padded_size(self.sz)?;
        if self.capacity < capacity {
            // This pointer uniquely owns its allocation. Failed realloc leaves the original valid.
            let ptr = NonNull::new(unsafe {
                ffi::sqlite3_realloc64(self.ptr.as_ptr().cast(), capacity as u64).cast::<u8>()
            })
            .ok_or_else(|| error_from_sqlite_code(ffi::SQLITE_NOMEM, None))?;
            self.ptr = ptr;
            self.capacity = capacity;
        }

        // Padding fits in the allocation after the visible data bytes.
        unsafe {
            self.ptr
                .as_ptr()
                .add(self.sz)
                .write_bytes(0, DESERIALIZE_PADDING);
        }
        Ok(())
    }

    fn into_raw(self) -> *mut u8 {
        let raw = self.ptr.as_ptr();
        std::mem::forget(self);
        raw
    }
}

impl Drop for OwnedData {
    fn drop(&mut self) {
        unsafe {
            ffi::sqlite3_free(self.ptr.as_ptr().cast());
        }
    }
}

/// Serialized database
pub enum Data<'conn> {
    /// Shared (`SQLITE_SERIALIZE_NOCOPY`) serialized database
    Shared(SharedData<'conn>),
    /// Owned serialized database
    Owned(OwnedData),
}

impl Deref for Data<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Data::Owned(od) => od.deref(),
            Data::Shared(sd) => sd.deref(),
        }
    }
}

impl Deref for SharedData<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.sz) }
    }
}
impl Deref for OwnedData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.sz) }
    }
}
impl DerefMut for OwnedData {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_mut(), self.sz) }
    }
}

impl Connection {
    /// Serialize a database.
    ///
    /// ```compile_fail
    /// use rusqlite::{Connection, Result, MAIN_DB};
    /// use rusqlite::serialize::Data;
    /// fn main() -> Result<()> {
    ///     let mut src = Connection::open_in_memory()?;
    ///     let data = src.serialize(MAIN_DB)?;
    ///     let mut dst = Connection::open_in_memory()?;
    ///     let Data::Owned(data) = data else {
    ///         panic!("expected OwnedData")
    ///     };
    ///     dst.deserialize(MAIN_DB, data, false)?;
    ///     let data = dst.serialize(MAIN_DB).unwrap(); // mutable borrow occurs here
    ///     dst.execute("INSERT INTO t VALUES (zeroblob(4096))", [])?; // <- immutable borrow occurs here
    ///     let Data::Shared(_) = data else { // mutable borrow later used here
    ///         panic!("expected SharedData")
    ///     };
    ///     Ok(())
    /// }
    /// ```
    pub fn serialize<N: Name>(&mut self, schema: N) -> Result<Data<'_>> {
        let schema = schema.as_cstr()?;
        let mut sz = 0;
        let ptr: *mut u8 = unsafe {
            ffi::sqlite3_serialize(
                self.handle(),
                schema.as_ptr(),
                &raw mut sz,
                ffi::SQLITE_SERIALIZE_NOCOPY,
            )
        };
        Ok(if let Some(ptr) = NonNull::new(ptr) {
            Data::Shared(SharedData {
                ptr,
                sz: serialized_size(sz)?,
                phantom: PhantomData,
            })
        } else {
            let ptr =
                unsafe { ffi::sqlite3_serialize(self.handle(), schema.as_ptr(), &raw mut sz, 0) };
            let ptr = NonNull::new(ptr)
                .ok_or_else(|| unsafe { error_from_handle(self.handle(), ffi::SQLITE_NOMEM) })?;
            // Own the allocation while validating SQLite's reported size.
            let mut data = OwnedData {
                ptr,
                sz: 0,
                capacity: 0,
            };
            data.sz = serialized_size(sz)?;
            data.capacity = data.sz;

            Data::Owned(data)
        })
    }

    /// Deserialize from stream
    pub fn deserialize_read_exact<N: Name, R: std::io::Read>(
        &mut self,
        schema: N,
        mut read: R,
        sz: usize,
        read_only: bool,
    ) -> Result<()> {
        let schema = schema.as_cstr()?;
        let mut data = OwnedData::zeroed(sz)?;
        read.read_exact(data.deref_mut())
            .map_err(Error::DeserializationReadFailure)?;

        self.deserialize(&*schema, data, read_only)
    }

    /// Copy serialized bytes into a read only database.
    ///
    /// The copy includes initialized padding that SQLite may read after malformed input.
    pub fn deserialize_bytes<N: Name>(&mut self, schema: N, data: &[u8]) -> Result<()> {
        let schema = schema.as_cstr()?;
        let mut owned = OwnedData::zeroed(data.len())?;
        owned.copy_from_slice(data);

        self.deserialize(&*schema, owned, true)
    }

    /// Deserialize a database.
    pub fn deserialize<N: Name>(
        &mut self,
        schema: N,
        mut data: OwnedData,
        read_only: bool,
    ) -> Result<()> {
        let schema = schema.as_cstr()?;
        let sz = deserialize_size(data.sz)?;
        data.ensure_padding()?;
        let capacity = deserialize_size(data.capacity)?;
        let flags = if read_only {
            ffi::SQLITE_DESERIALIZE_FREEONCLOSE | ffi::SQLITE_DESERIALIZE_READONLY
        } else {
            ffi::SQLITE_DESERIALIZE_FREEONCLOSE | ffi::SQLITE_DESERIALIZE_RESIZEABLE
        };

        // FREEONCLOSE transfers ownership to SQLite, including when the native call fails.
        let data = data.into_raw();
        self.deserialize_(&schema, data, sz, capacity, flags)
        /* TODO
        if let Some(mxSize) = mxSize {
            unsafe {
                ffi::sqlite3_file_control(
                    self.handle(),
                    schema.as_ptr(),
                    ffi::SQLITE_FCNTL_SIZE_LIMIT,
                    &mut mxSize,
                )
            };
        }*/
    }

    fn deserialize_(
        &mut self,
        schema: &CStr,
        data: *mut u8,
        sz: ffi::sqlite_int64,
        capacity: ffi::sqlite_int64,
        flags: std::ffi::c_uint,
    ) -> Result<()> {
        let rc = unsafe {
            ffi::sqlite3_deserialize(self.handle(), schema.as_ptr(), data, sz, capacity, flags)
        };
        if rc != ffi::SQLITE_OK {
            return Err(unsafe { error_from_handle(self.handle(), rc) });
        }
        Ok(())
    }
}

fn serialized_size(sz: ffi::sqlite_int64) -> Result<usize> {
    let size = usize::try_from(sz).map_err(|_| {
        err!(
            ffi::SQLITE_TOOBIG,
            "Serialized database size {sz} exceeds the slice size limit"
        )
    })?;
    deserialize_size(size)?;
    Ok(size)
}

fn padded_size(sz: usize) -> Result<usize> {
    let capacity = sz.checked_add(DESERIALIZE_PADDING).ok_or_else(|| {
        err!(
            ffi::SQLITE_TOOBIG,
            "Serialized database size {sz} leaves no room for padding"
        )
    })?;
    deserialize_size(capacity)?;
    Ok(capacity)
}

fn deserialize_size(sz: usize) -> Result<ffi::sqlite_int64> {
    if sz > isize::MAX as usize {
        return Err(err!(
            ffi::SQLITE_TOOBIG,
            "Serialized database size {sz} exceeds the slice size limit"
        ));
    }

    ffi::sqlite_int64::try_from(sz).map_err(|_| {
        err!(
            ffi::SQLITE_TOOBIG,
            "Serialized database size {sz} exceeds SQLite's size limit"
        )
    })
}

#[cfg(all(test, not(miri)))]
mod test {
    use std::assert_matches;

    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;
    use crate::MAIN_DB;

    #[test]
    fn serialize() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE x AS SELECT 'data'")?;
        let data = db.serialize(MAIN_DB)?;
        let Data::Owned(data) = data else {
            panic!("expected OwnedData")
        };
        assert!(data.sz > 0);
        Ok(())
    }

    #[test]
    fn deserialize_read_exact() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE x AS SELECT 'data'")?;
        let data = db.serialize(MAIN_DB)?;

        let mut dst = Connection::open_in_memory()?;
        let read = data.deref();
        dst.deserialize_read_exact(MAIN_DB, read, read.len(), false)?;
        dst.execute("DELETE FROM x", [])?;
        Ok(())
    }

    #[test]
    fn deserialize_short_read() -> Result<()> {
        let mut dst = Connection::open_in_memory()?;
        let read = b"";
        assert_matches!(
            dst.deserialize_read_exact(MAIN_DB, &read[..], 1024, true).unwrap_err(),
            Error::DeserializationReadFailure(error) if error.kind() == std::io::ErrorKind::UnexpectedEof
        );
        Ok(())
    }

    #[test]
    fn deserialize_bytes() -> Result<()> {
        let data = b"";
        let mut dst = Connection::open_in_memory()?;
        dst.deserialize_bytes(MAIN_DB, data)?;
        Ok(())
    }

    #[test]
    fn deserialize() -> Result<()> {
        let mut src = Connection::open_in_memory()?;
        src.execute_batch("CREATE TABLE x AS SELECT 'data'")?;
        let data = src.serialize(MAIN_DB)?;
        let Data::Owned(data) = data else {
            panic!("expected OwnedData")
        };

        let mut dst = Connection::open_in_memory()?;
        dst.deserialize(MAIN_DB, data, false)?;
        dst.execute("DELETE FROM x", [])?;
        Ok(())
    }
}
