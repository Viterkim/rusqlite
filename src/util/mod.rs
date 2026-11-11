// Internal utilities
pub(crate) mod param_cache;
mod small_cstr;
pub(crate) use param_cache::ParamIndexCache;
pub(crate) use small_cstr::SmallCString;

// Doesn't use any modern features or vtab stuff, but is only used by them.
mod sqlite_string;
pub(crate) use sqlite_string::{SqliteMallocString, alloc};

// Check the tail without preparing it, since PRAGMAs can act during preparation.
pub(crate) fn sql_is_empty(sql: &[u8]) -> bool {
    let end = sql.iter().position(|&byte| byte == 0).unwrap_or(sql.len());
    let mut sql = &sql[..end];

    loop {
        match sql {
            [] => return true,
            [b' ' | b'\t' | b'\n' | b'\r' | b'\x0c', ..] => {
                // SQLite consumes vertical tabs after another whitespace byte.
                let end = sql
                    .iter()
                    .position(|&byte| !matches!(byte, b' ' | b'\t'..=b'\r'))
                    .unwrap_or(sql.len());
                sql = &sql[end..];
            }
            [b';', rest @ ..] | [0xef, 0xbb, 0xbf, rest @ ..] => sql = rest,
            [b'-', b'-', rest @ ..] => {
                let end = rest
                    .iter()
                    .position(|&byte| byte == b'\n')
                    .unwrap_or(rest.len());
                sql = &rest[end..];
            }
            [b'/', b'*', rest @ ..] if !rest.is_empty() => {
                let Some(end) = rest.windows(2).position(|bytes| bytes == b"*/") else {
                    return true;
                };

                sql = &rest[end + 2..];
            }
            _ => return false,
        }
    }
}

pub(crate) unsafe extern "C" fn free_boxed_value<T>(p: *mut std::ffi::c_void) {
    drop(unsafe { Box::from_raw(p.cast::<T>()) });
}

pub(crate) unsafe fn from_raw_parts<'a, T>(data: *const T, len: std::ffi::c_int) -> &'a [T] {
    if len > 0 {
        return unsafe { std::slice::from_raw_parts(data, len as usize) };
    }
    &[]
}
#[cfg(any(feature = "session", feature = "vtab"))]
pub(crate) unsafe fn from_raw_parts_mut<'a, T>(data: *mut T, len: std::ffi::c_int) -> &'a mut [T] {
    if len > 0 {
        return unsafe { std::slice::from_raw_parts_mut(data, len as usize) };
    }
    &mut []
}

use crate::Result;
use std::ffi::CStr;

pub enum Named<'a> {
    Small(SmallCString),
    C(&'a CStr),
}
impl std::ops::Deref for Named<'_> {
    type Target = CStr;

    #[inline]
    fn deref(&self) -> &CStr {
        match self {
            Named::Small(s) => s.as_cstr(),
            Named::C(s) => s,
        }
    }
}

/// Database, table, column, collation, function, module, vfs name
pub trait Name: std::fmt::Debug + Copy {
    /// As C string
    fn as_cstr(&self) -> Result<Named<'_>>;
}
impl Name for &str {
    fn as_cstr(&self) -> Result<Named<'_>> {
        let ss = SmallCString::new(self)?;
        Ok(Named::Small(ss))
    }
}
impl Name for &CStr {
    #[inline]
    fn as_cstr(&self) -> Result<Named<'_>> {
        Ok(Named::C(self))
    }
}

#[cfg(test)]
mod test {
    use std::ops::Deref as _;

    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    #[test]
    fn from_raw_parts() {
        let slice = unsafe { super::from_raw_parts(std::ptr::null::<u8>(), 0) };
        assert!(slice.is_empty());
    }
    #[test]
    fn str_as_cstr() -> crate::Result<()> {
        use super::Name as _;
        let named = "foo".as_cstr()?;
        let cstr = named.deref();
        assert_eq!(cstr, c"foo");
        Ok(())
    }
}
