//! [Session Extension](https://sqlite.org/sessionintro.html)
#![expect(non_camel_case_types)]

use std::ffi::{CStr, c_char, c_int, c_uchar, c_void};
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::panic::catch_unwind;
use std::ptr;
use std::slice::{from_raw_parts, from_raw_parts_mut};

use fallible_streaming_iterator::FallibleStreamingIterator;

use crate::error::{Error, check, error_from_sqlite_code};
use crate::ffi;
use crate::hooks::Action;
use crate::types::ValueRef;
use crate::{Connection, MAIN_DB, Name, Result, errmsg_to_string};

// https://sqlite.org/session.html

type Filter = Option<Box<dyn Fn(&str) -> bool>>;

/// An instance of this object is a session that can be
/// used to record changes to a database.
pub struct Session<'conn> {
    phantom: PhantomData<&'conn Connection>,
    s: *mut ffi::sqlite3_session,
    filter: Filter,
}

impl Session<'_> {
    /// Create a new session object
    #[inline]
    pub fn new(db: &Connection) -> Result<Session<'_>> {
        Session::new_with_name(db, MAIN_DB)
    }

    /// Create a new session object
    #[inline]
    pub fn new_with_name<N: Name>(db: &Connection, name: N) -> Result<Session<'_>> {
        let name = name.as_cstr()?;

        let db = db.db.borrow_mut().db;

        let mut s: *mut ffi::sqlite3_session = ptr::null_mut();
        check(unsafe { ffi::sqlite3session_create(db, name.as_ptr(), &mut s) })?;

        Ok(Session {
            phantom: PhantomData,
            s,
            filter: None,
        })
    }

    /// Set a table filter
    pub fn table_filter<F>(&mut self, filter: Option<F>)
    where
        F: Fn(&str) -> bool + Send + 'static,
    {
        unsafe extern "C" fn call_boxed_closure<F>(
            p_arg: *mut c_void,
            tbl_str: *const c_char,
        ) -> c_int
        where
            F: Fn(&str) -> bool,
        {
            unsafe {
                let tbl_name = CStr::from_ptr(tbl_str).to_str();
                c_int::from(
                    catch_unwind(|| {
                        let boxed_filter: *mut F = p_arg.cast::<F>();
                        (*boxed_filter)(tbl_name.expect("non-utf8 table name"))
                    })
                    .unwrap_or_default(),
                )
            }
        }

        match filter {
            Some(filter) => {
                let boxed_filter = Box::new(filter);
                unsafe {
                    ffi::sqlite3session_table_filter(
                        self.s,
                        Some(call_boxed_closure::<F>),
                        &*boxed_filter as *const F as *mut _,
                    );
                }
                self.filter = Some(boxed_filter);
            }
            _ => {
                unsafe { ffi::sqlite3session_table_filter(self.s, None, ptr::null_mut()) }
                self.filter = None;
            }
        }
    }

    /// Attach a table. `None` means all tables.
    pub fn attach<N: Name>(&mut self, table: Option<N>) -> Result<()> {
        let cs = table.as_ref().map(N::as_cstr).transpose()?;
        let table = cs.as_ref().map(|s| s.as_ptr()).unwrap_or(ptr::null());
        check(unsafe { ffi::sqlite3session_attach(self.s, table) })
    }

    /// Enable or disable estimating the changeset size.
    ///
    /// This adds overhead while recording changes and must be set before
    /// attaching the first table.
    pub fn set_changeset_size_tracking(&mut self, enabled: bool) -> Result<()> {
        let mut value = c_int::from(enabled);
        check(unsafe {
            ffi::sqlite3session_object_config(
                self.s,
                ffi::SQLITE_SESSION_OBJCONFIG_SIZE,
                (&mut value as *mut c_int).cast(),
            )
        })
    }

    /// Return an upper bound in bytes for the current changeset size.
    ///
    /// Returns zero unless [`Session::set_changeset_size_tracking`] was enabled.
    pub fn changeset_size(&self) -> i64 {
        unsafe { ffi::sqlite3session_changeset_size(self.s) }
    }

    /// Generate a Changeset
    pub fn changeset(&mut self) -> Result<Changeset> {
        let mut n = 0;
        let mut cs: *mut c_void = ptr::null_mut();
        check(unsafe { ffi::sqlite3session_changeset(self.s, &mut n, &mut cs) })?;
        Ok(Changeset { cs, n })
    }

    /// Write the set of changes represented by this session to `output`.
    #[inline]
    pub fn changeset_strm(&mut self, output: &mut dyn Write) -> Result<()> {
        let output_ref = &output;
        check(unsafe {
            ffi::sqlite3session_changeset_strm(
                self.s,
                Some(x_output),
                output_ref as *const &mut dyn Write as *mut c_void,
            )
        })
    }

    /// Generate a Patchset
    #[inline]
    pub fn patchset(&mut self) -> Result<Changeset> {
        let mut n = 0;
        let mut ps: *mut c_void = ptr::null_mut();
        check(unsafe { ffi::sqlite3session_patchset(self.s, &mut n, &mut ps) })?;
        // TODO Validate: same struct
        Ok(Changeset { cs: ps, n })
    }

    /// Write the set of patches represented by this session to `output`.
    #[inline]
    pub fn patchset_strm(&mut self, output: &mut dyn Write) -> Result<()> {
        let output_ref = &output;
        check(unsafe {
            ffi::sqlite3session_patchset_strm(
                self.s,
                Some(x_output),
                output_ref as *const &mut dyn Write as *mut c_void,
            )
        })
    }

    /// Load the difference between tables.
    pub fn diff<N: Name>(&mut self, from: N, table: N) -> Result<()> {
        let from = from.as_cstr()?;
        let table = table.as_cstr()?;
        let table = table.as_ptr();
        unsafe {
            let mut errmsg = ptr::null_mut();
            let r =
                ffi::sqlite3session_diff(self.s, from.as_ptr(), table, &mut errmsg as *mut *mut _);
            if r != ffi::SQLITE_OK {
                let errmsg: *mut c_char = errmsg;
                let message = errmsg_to_string(&*errmsg);
                ffi::sqlite3_free(errmsg as *mut c_void);
                return Err(error_from_sqlite_code(r, Some(message)));
            }
        }
        Ok(())
    }

    /// Test if a changeset has recorded any changes
    #[inline]
    pub fn is_empty(&self) -> bool {
        unsafe { ffi::sqlite3session_isempty(self.s) != 0 }
    }

    /// Query the current state of the session
    #[inline]
    pub fn is_enabled(&self) -> bool {
        unsafe { ffi::sqlite3session_enable(self.s, -1) != 0 }
    }

    /// Enable or disable the recording of changes
    #[inline]
    pub fn set_enabled(&mut self, enabled: bool) {
        unsafe {
            ffi::sqlite3session_enable(self.s, c_int::from(enabled));
        }
    }

    /// Query the current state of the indirect flag
    #[inline]
    pub fn is_indirect(&self) -> bool {
        unsafe { ffi::sqlite3session_indirect(self.s, -1) != 0 }
    }

    /// Set or clear the indirect change flag
    #[inline]
    pub fn set_indirect(&mut self, indirect: bool) {
        unsafe {
            ffi::sqlite3session_indirect(self.s, c_int::from(indirect));
        }
    }
}

impl Drop for Session<'_> {
    #[inline]
    fn drop(&mut self) {
        if self.filter.is_some() {
            self.table_filter(None::<fn(&str) -> bool>);
        }
        unsafe { ffi::sqlite3session_delete(self.s) };
    }
}

/// Invert a changeset
#[inline]
pub fn invert_strm(input: &mut dyn Read, output: &mut dyn Write) -> Result<()> {
    let input_ref = &input;
    let output_ref = &output;
    check(unsafe {
        ffi::sqlite3changeset_invert_strm(
            Some(x_input),
            input_ref as *const &mut dyn Read as *mut c_void,
            Some(x_output),
            output_ref as *const &mut dyn Write as *mut c_void,
        )
    })
}

/// Combine two changesets
#[inline]
pub fn concat_strm(
    input_a: &mut dyn Read,
    input_b: &mut dyn Read,
    output: &mut dyn Write,
) -> Result<()> {
    let input_a_ref = &input_a;
    let input_b_ref = &input_b;
    let output_ref = &output;
    check(unsafe {
        ffi::sqlite3changeset_concat_strm(
            Some(x_input),
            input_a_ref as *const &mut dyn Read as *mut c_void,
            Some(x_input),
            input_b_ref as *const &mut dyn Read as *mut c_void,
            Some(x_output),
            output_ref as *const &mut dyn Write as *mut c_void,
        )
    })
}

/// Changeset or Patchset
pub struct Changeset {
    cs: *mut c_void,
    n: c_int,
}

impl Changeset {
    /// Invert a changeset
    #[inline]
    pub fn invert(&self) -> Result<Changeset> {
        let mut n = 0;
        let mut cs = ptr::null_mut();
        check(unsafe {
            ffi::sqlite3changeset_invert(self.n, self.cs, &mut n, &mut cs as *mut *mut _)
        })?;
        Ok(Changeset { cs, n })
    }

    /// Create an iterator to traverse a changeset
    #[inline]
    pub fn iter(&self) -> Result<ChangesetIter<'_>> {
        let mut it = ptr::null_mut();
        check(unsafe { ffi::sqlite3changeset_start(&mut it as *mut *mut _, self.n, self.cs) })?;
        Ok(ChangesetIter {
            phantom: PhantomData,
            it,
            item: None,
            _input: None,
        })
    }

    /// Concatenate two changeset objects
    #[inline]
    pub fn concat(a: &Changeset, b: &Changeset) -> Result<Changeset> {
        let mut n = 0;
        let mut cs = ptr::null_mut();
        check(unsafe {
            ffi::sqlite3changeset_concat(a.n, a.cs, b.n, b.cs, &mut n, &mut cs as *mut *mut _)
        })?;
        Ok(Changeset { cs, n })
    }
}

impl Drop for Changeset {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            ffi::sqlite3_free(self.cs);
        }
    }
}

/// Cursor for iterating over the elements of a changeset
/// or patchset.
pub struct ChangesetIter<'changeset> {
    phantom: PhantomData<&'changeset [u8]>,
    it: *mut ffi::sqlite3_changeset_iter,
    item: Option<ChangesetItem<'changeset>>,
    #[expect(
        clippy::redundant_allocation,
        reason = "SQLite retains a pointer to this stable reference slot"
    )]
    _input: Option<Box<&'changeset mut dyn Read>>,
}

impl ChangesetIter<'_> {
    /// Iterate over a changeset stored in a borrowed byte slice.
    ///
    /// The iterator cannot outlive its input bytes:
    ///
    /// ```compile_fail
    /// use rusqlite::session::ChangesetIter;
    ///
    /// let iter = {
    ///     let bytes = vec![0_u8; 16];
    ///     ChangesetIter::start_bytes(&bytes).unwrap()
    /// };
    /// drop(iter);
    /// ```
    pub fn start_bytes(input: &[u8]) -> Result<ChangesetIter<'_>> {
        let len = c_int::try_from(input.len())
            .map_err(|_| error_from_sqlite_code(ffi::SQLITE_TOOBIG, None))?;
        let mut it = ptr::null_mut();
        check(unsafe {
            ffi::sqlite3changeset_start(&mut it, len, input.as_ptr().cast_mut().cast())
        })?;
        Ok(ChangesetIter {
            phantom: PhantomData,
            it,
            item: None,
            _input: None,
        })
    }

    /// Create an iterator on `input`.
    ///
    /// The iterator owns the callback context SQLite reads during iteration.
    /// It cannot outlive the reader:
    ///
    /// ```compile_fail
    /// use rusqlite::session::ChangesetIter;
    ///
    /// let iter = {
    ///     let mut bytes = &[0_u8; 16][..];
    ///     ChangesetIter::start_strm(&mut bytes).unwrap()
    /// };
    /// drop(iter);
    /// ```
    pub fn start_strm(input: &mut dyn Read) -> Result<ChangesetIter<'_>> {
        let mut input = Box::new(input);
        let mut it = ptr::null_mut();
        // The boxed reference slot stays at a stable address until SQLite finalizes the iterator.
        check(unsafe {
            ffi::sqlite3changeset_start_strm(
                &mut it as *mut *mut _,
                Some(x_input),
                (&mut *input as *mut &mut dyn Read).cast(),
            )
        })?;
        Ok(ChangesetIter {
            phantom: PhantomData,
            it,
            item: None,
            _input: Some(input),
        })
    }

    /// Advance to and return the next change, if any.
    #[expect(
        clippy::should_implement_trait,
        reason = "streaming items borrow the iterator, so Iterator cannot represent this method"
    )]
    pub fn next(&mut self) -> Result<Option<&ChangesetItem<'_>>> {
        FallibleStreamingIterator::next(self)
    }
}

impl<'changeset> FallibleStreamingIterator for ChangesetIter<'changeset> {
    type Error = Error;
    type Item = ChangesetItem<'changeset>;

    #[inline]
    fn advance(&mut self) -> Result<()> {
        let rc = unsafe { ffi::sqlite3changeset_next(self.it) };
        match rc {
            ffi::SQLITE_ROW => {
                self.item = Some(ChangesetItem {
                    it: self.it,
                    phantom: PhantomData,
                });
                Ok(())
            }
            ffi::SQLITE_DONE => {
                self.item = None;
                Ok(())
            }
            code => Err(error_from_sqlite_code(code, None)),
        }
    }

    #[inline]
    fn get(&self) -> Option<&ChangesetItem<'changeset>> {
        self.item.as_ref()
    }
}

/// Operation
pub struct Operation<'item> {
    table_name: &'item str,
    number_of_columns: i32,
    code: Action,
    indirect: bool,
}
impl Operation<'_> {
    /// Returns the table name.
    #[inline]
    pub fn table_name(&self) -> &str {
        self.table_name
    }

    /// Returns the number of columns in table
    #[inline]
    pub fn number_of_columns(&self) -> i32 {
        self.number_of_columns
    }

    /// Returns the action code.
    #[inline]
    pub fn code(&self) -> Action {
        self.code
    }

    /// Return the changeset operation.
    pub fn changeset_operation(&self) -> Result<ChangesetOperation> {
        match self.code {
            Action::SQLITE_INSERT => Ok(ChangesetOperation::Insert),
            Action::SQLITE_UPDATE => Ok(ChangesetOperation::Update),
            Action::SQLITE_DELETE => Ok(ChangesetOperation::Delete),
            Action::UNKNOWN => Err(error_from_sqlite_code(ffi::SQLITE_CORRUPT, None)),
        }
    }

    /// Return the table's column count as a nonnegative integer.
    pub fn column_count(&self) -> Result<usize> {
        usize::try_from(self.number_of_columns)
            .map_err(|_| error_from_sqlite_code(ffi::SQLITE_CORRUPT, None))
    }

    /// Returns `true` for an 'indirect' change.
    #[inline]
    pub fn indirect(&self) -> bool {
        self.indirect
    }
}

/// Operation represented by a changeset row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangesetOperation {
    /// An inserted row.
    Insert,
    /// An updated row.
    Update,
    /// A deleted row.
    Delete,
}

impl Drop for ChangesetIter<'_> {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            ffi::sqlite3changeset_finalize(self.it);
        }
    }
}

/// An item passed to a conflict-handler by
/// [`Connection::apply`](Connection::apply), or an item generated by
/// [`ChangesetIter::next`](ChangesetIter::next).
///
/// Conflict callback items cannot escape the callback:
///
/// ```compile_fail
/// use rusqlite::session::{ChangesetItem, ConflictAction};
/// use rusqlite::Connection;
///
/// # fn check(db: &Connection, changeset: &rusqlite::session::Changeset) {
/// let mut escaped: Option<ChangesetItem<'_>> = None;
/// db.apply(changeset, None::<fn(&str) -> bool>, |_, item| {
///     escaped = Some(item);
///     ConflictAction::SQLITE_CHANGESET_ABORT
/// });
/// # }
/// ```
// TODO enum ? Delete, Insert, Update, ...
pub struct ChangesetItem<'a> {
    it: *mut ffi::sqlite3_changeset_iter,
    phantom: PhantomData<&'a ffi::sqlite3_changeset_iter>,
}

impl ChangesetItem<'_> {
    /// Return a conflicting column value, if present.
    ///
    /// Only valid during a DATA or CONFLICT callback.
    pub fn conflict_value_opt(&self, col: usize) -> Result<Option<ValueRef<'_>>> {
        let col = c_int::try_from(col).map_err(|_| Error::InvalidColumnIndex(col))?;
        let mut value = ptr::null_mut();
        check(unsafe { ffi::sqlite3changeset_conflict(self.it, col, &mut value) })?;
        Ok((!value.is_null()).then(|| unsafe { ValueRef::from_value(value) }))
    }

    /// Return a new column value, if present in the changeset.
    pub fn new_value_opt(&self, col: usize) -> Result<Option<ValueRef<'_>>> {
        let col = c_int::try_from(col).map_err(|_| Error::InvalidColumnIndex(col))?;
        let mut value = ptr::null_mut();
        check(unsafe { ffi::sqlite3changeset_new(self.it, col, &mut value) })?;
        Ok((!value.is_null()).then(|| unsafe { ValueRef::from_value(value) }))
    }

    /// Return an old column value, if present in the changeset.
    pub fn old_value_opt(&self, col: usize) -> Result<Option<ValueRef<'_>>> {
        let col = c_int::try_from(col).map_err(|_| Error::InvalidColumnIndex(col))?;
        let mut value = ptr::null_mut();
        check(unsafe { ffi::sqlite3changeset_old(self.it, col, &mut value) })?;
        Ok((!value.is_null()).then(|| unsafe { ValueRef::from_value(value) }))
    }

    /// Obtain conflicting row values
    ///
    /// May only be called with an `SQLITE_CHANGESET_DATA` or
    /// `SQLITE_CHANGESET_CONFLICT` conflict handler callback.
    #[inline]
    pub fn conflict(&self, col: usize) -> Result<ValueRef<'_>> {
        unsafe {
            let mut p_value: *mut ffi::sqlite3_value = ptr::null_mut();
            check(ffi::sqlite3changeset_conflict(
                self.it,
                col as i32,
                &mut p_value,
            ))?;
            if p_value.is_null() {
                Err(Error::InvalidColumnIndex(col))
            } else {
                Ok(ValueRef::from_value(p_value))
            }
        }
    }

    /// Determine the number of foreign key constraint violations
    ///
    /// May only be called with an `SQLITE_CHANGESET_FOREIGN_KEY` conflict
    /// handler callback.
    #[inline]
    pub fn fk_conflicts(&self) -> Result<i32> {
        unsafe {
            let mut p_out = 0;
            check(ffi::sqlite3changeset_fk_conflicts(self.it, &mut p_out))?;
            Ok(p_out)
        }
    }

    /// Obtain new.* Values
    ///
    /// May only be called if the type of change is either `SQLITE_UPDATE` or
    /// `SQLITE_INSERT`.
    #[inline]
    pub fn new_value(&self, col: usize) -> Result<ValueRef<'_>> {
        unsafe {
            let mut p_value: *mut ffi::sqlite3_value = ptr::null_mut();
            check(ffi::sqlite3changeset_new(self.it, col as i32, &mut p_value))?;
            if p_value.is_null() {
                Err(Error::InvalidColumnIndex(col))
            } else {
                Ok(ValueRef::from_value(p_value))
            }
        }
    }

    /// Obtain old.* Values
    ///
    /// May only be called if the type of change is either `SQLITE_DELETE` or
    /// `SQLITE_UPDATE`.
    #[inline]
    pub fn old_value(&self, col: usize) -> Result<ValueRef<'_>> {
        unsafe {
            let mut p_value: *mut ffi::sqlite3_value = ptr::null_mut();
            check(ffi::sqlite3changeset_old(self.it, col as i32, &mut p_value))?;
            if p_value.is_null() {
                Err(Error::InvalidColumnIndex(col))
            } else {
                Ok(ValueRef::from_value(p_value))
            }
        }
    }

    /// Obtain the current operation
    #[inline]
    pub fn op(&self) -> Result<Operation<'_>> {
        let mut number_of_columns = 0;
        let mut code = 0;
        let mut indirect = 0;
        let tab = unsafe {
            let mut pz_tab: *const c_char = ptr::null();
            check(ffi::sqlite3changeset_op(
                self.it,
                &mut pz_tab,
                &mut number_of_columns,
                &mut code,
                &mut indirect,
            ))?;
            CStr::from_ptr(pz_tab)
        };
        let table_name = tab.to_str()?;
        Ok(Operation {
            table_name,
            number_of_columns,
            code: Action::from(code),
            indirect: indirect != 0,
        })
    }

    /// Obtain the primary key definition of a table
    #[inline]
    pub fn pk(&self) -> Result<&[u8]> {
        let mut number_of_columns = 0;
        unsafe {
            let mut pks: *mut c_uchar = ptr::null_mut();
            check(ffi::sqlite3changeset_pk(
                self.it,
                &mut pks,
                &mut number_of_columns,
            ))?;
            Ok(from_raw_parts(pks, number_of_columns as usize))
        }
    }
}

/// Used to combine two or more changesets or
/// patchsets
pub struct Changegroup {
    cg: *mut ffi::sqlite3_changegroup,
}

impl Changegroup {
    /// Create a new change group.
    #[inline]
    pub fn new() -> Result<Self> {
        let mut cg = ptr::null_mut();
        check(unsafe { ffi::sqlite3changegroup_new(&mut cg) })?;
        Ok(Changegroup { cg })
    }

    /// Add a changeset
    #[inline]
    pub fn add(&mut self, cs: &Changeset) -> Result<()> {
        check(unsafe { ffi::sqlite3changegroup_add(self.cg, cs.n, cs.cs) })
    }

    /// Add a changeset read from `input` to this change group.
    #[inline]
    pub fn add_stream(&mut self, input: &mut dyn Read) -> Result<()> {
        let input_ref = &input;
        check(unsafe {
            ffi::sqlite3changegroup_add_strm(
                self.cg,
                Some(x_input),
                input_ref as *const &mut dyn Read as *mut c_void,
            )
        })
    }

    /// Obtain a composite Changeset
    #[inline]
    pub fn output(&mut self) -> Result<Changeset> {
        let mut n = 0;
        let mut output: *mut c_void = ptr::null_mut();
        check(unsafe { ffi::sqlite3changegroup_output(self.cg, &mut n, &mut output) })?;
        Ok(Changeset { cs: output, n })
    }

    /// Write the combined set of changes to `output`.
    #[inline]
    pub fn output_strm(&mut self, output: &mut dyn Write) -> Result<()> {
        let output_ref = &output;
        check(unsafe {
            ffi::sqlite3changegroup_output_strm(
                self.cg,
                Some(x_output),
                output_ref as *const &mut dyn Write as *mut c_void,
            )
        })
    }
}

impl Drop for Changegroup {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            ffi::sqlite3changegroup_delete(self.cg);
        }
    }
}

/// Rebase local changesets after applying conflicting remote changesets.
///
/// SQLite's rebaser API is experimental.
pub struct Rebaser {
    rebaser: *mut ffi::sqlite3_rebaser,
}
impl Rebaser {
    /// Create a new rebaser.
    pub fn new() -> Result<Self> {
        let mut rebaser = ptr::null_mut();
        check(unsafe { ffi::sqlite3rebaser_create(&mut rebaser) })?;
        Ok(Self { rebaser })
    }

    /// Configure with rebase data returned by a v2 changeset apply.
    ///
    /// When multiple remote changesets are applied, configure in the order
    /// they were applied.
    pub fn configure(&mut self, rebase: &[u8]) -> Result<()> {
        let len = c_int::try_from(rebase.len())
            .map_err(|_| error_from_sqlite_code(ffi::SQLITE_TOOBIG, None))?;
        check(unsafe { ffi::sqlite3rebaser_configure(self.rebaser, len, rebase.as_ptr().cast()) })
    }

    /// Rebase a buffered local changeset.
    pub fn rebase(&mut self, changeset: &Changeset) -> Result<Changeset> {
        let mut output = RebaseBuffer {
            ptr: ptr::null_mut(),
            len: 0,
        };
        check(unsafe {
            ffi::sqlite3rebaser_rebase(
                self.rebaser,
                changeset.n,
                changeset.cs,
                &mut output.len,
                &mut output.ptr,
            )
        })?;
        let changeset = Changeset {
            cs: output.ptr,
            n: output.len,
        };
        output.ptr = ptr::null_mut();
        Ok(changeset)
    }

    /// Rebase a streamed local changeset into `output`.
    pub fn rebase_strm(&mut self, input: &mut dyn Read, output: &mut dyn Write) -> Result<()> {
        let input_ref = &input;
        let output_ref = &output;
        check(unsafe {
            ffi::sqlite3rebaser_rebase_strm(
                self.rebaser,
                Some(x_input),
                input_ref as *const &mut dyn Read as *mut c_void,
                Some(x_output),
                output_ref as *const &mut dyn Write as *mut c_void,
            )
        })
    }
}
impl Drop for Rebaser {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3rebaser_delete(self.rebaser) }
    }
}

bitflags::bitflags! {
    /// Flags for applying a changeset with [`Connection::apply_with_flags`] or
    /// [`Connection::apply_strm_with_flags`].
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ChangesetApplyFlags: c_int {
        /// Do not create a savepoint around the changeset.
        const NOSAVEPOINT = ffi::SQLITE_CHANGESETAPPLY_NOSAVEPOINT;
        /// Apply the inverse of the changeset.
        const INVERT = ffi::SQLITE_CHANGESETAPPLY_INVERT;
        /// Skip updates that do not change any values.
        const IGNORENOOP = ffi::SQLITE_CHANGESETAPPLY_IGNORENOOP;
        /// Treat foreign key actions as NO ACTION while applying.
        const FKNOACTION = ffi::SQLITE_CHANGESETAPPLY_FKNOACTION;
    }
}

struct RebaseBuffer {
    ptr: *mut c_void,
    len: c_int,
}
impl RebaseBuffer {
    fn to_vec(&self) -> Option<Vec<u8>> {
        if self.ptr.is_null() {
            None
        } else {
            // SQLite returns a buffer of len bytes, which remains valid until sqlite3_free.
            Some(unsafe { from_raw_parts(self.ptr.cast(), self.len as usize) }.to_vec())
        }
    }
}
impl Drop for RebaseBuffer {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_free(self.ptr) }
    }
}

impl Connection {
    /// Apply a changeset to a database
    pub fn apply<F, C>(&self, cs: &Changeset, filter: Option<F>, conflict: C) -> Result<()>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        let db = self.db.borrow_mut().db;

        self.with_apply_callbacks(filter, conflict, true, |filtered, context| {
            check(unsafe {
                if filtered {
                    ffi::sqlite3changeset_apply(
                        db,
                        cs.n,
                        cs.cs,
                        Some(call_filter::<F, C>),
                        Some(call_conflict::<F, C>),
                        context,
                    )
                } else {
                    ffi::sqlite3changeset_apply(
                        db,
                        cs.n,
                        cs.cs,
                        None,
                        Some(call_conflict::<F, C>),
                        context,
                    )
                }
            })
        })
    }

    /// Apply a changeset with SQLite's experimental v2 flags.
    ///
    /// Any rebase output is discarded.
    pub fn apply_with_flags<F, C>(
        &self,
        cs: &Changeset,
        filter: Option<F>,
        conflict: C,
        flags: ChangesetApplyFlags,
    ) -> Result<()>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        self.apply_with_flags_impl(cs.n, cs.cs, filter, conflict, flags, false)?;
        Ok(())
    }

    /// Apply a borrowed changeset with SQLite's experimental v2 flags.
    ///
    /// Any rebase output is discarded. The bytes remain borrowed for the
    /// duration of the call.
    pub fn apply_bytes_with_flags<F, C>(
        &self,
        bytes: &[u8],
        filter: Option<F>,
        conflict: C,
        flags: ChangesetApplyFlags,
    ) -> Result<()>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        let len = c_int::try_from(bytes.len())
            .map_err(|_| error_from_sqlite_code(ffi::SQLITE_TOOBIG, None))?;
        self.apply_with_flags_impl(
            len,
            bytes.as_ptr().cast_mut().cast(),
            filter,
            conflict,
            flags,
            false,
        )?;
        Ok(())
    }

    /// Apply a changeset with SQLite's experimental v2 flags and return any
    /// rebase data generated when conflicts are resolved.
    ///
    /// The rebase data can be passed to [`Rebaser::configure`].
    pub fn apply_with_flags_and_rebase<F, C>(
        &self,
        cs: &Changeset,
        filter: Option<F>,
        conflict: C,
        flags: ChangesetApplyFlags,
    ) -> Result<Option<Vec<u8>>>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        self.apply_with_flags_impl(cs.n, cs.cs, filter, conflict, flags, true)
    }

    fn apply_with_flags_impl<F, C>(
        &self,
        len: c_int,
        bytes: *mut c_void,
        filter: Option<F>,
        conflict: C,
        flags: ChangesetApplyFlags,
        rebase: bool,
    ) -> Result<Option<Vec<u8>>>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        let db = self.db.borrow_mut().db;
        let mut output = RebaseBuffer {
            ptr: ptr::null_mut(),
            len: 0,
        };
        self.with_apply_callbacks(
            filter,
            conflict,
            !flags.contains(ChangesetApplyFlags::NOSAVEPOINT),
            |filtered, context| {
                check(unsafe {
                    ffi::sqlite3changeset_apply_v2(
                        db,
                        len,
                        bytes,
                        if filtered {
                            Some(call_filter::<F, C>)
                        } else {
                            None
                        },
                        Some(call_conflict::<F, C>),
                        context,
                        if rebase {
                            &mut output.ptr
                        } else {
                            ptr::null_mut()
                        },
                        if rebase {
                            &mut output.len
                        } else {
                            ptr::null_mut()
                        },
                        flags.bits(),
                    )
                })
            },
        )?;
        Ok(output.to_vec())
    }

    /// Apply a changeset to a database
    pub fn apply_strm<F, C>(
        &self,
        input: &mut dyn Read,
        filter: Option<F>,
        conflict: C,
    ) -> Result<()>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        let input_ref = &input;
        let db = self.db.borrow_mut().db;

        self.with_apply_callbacks(filter, conflict, true, |filtered, context| {
            check(unsafe {
                if filtered {
                    ffi::sqlite3changeset_apply_strm(
                        db,
                        Some(x_input),
                        input_ref as *const &mut dyn Read as *mut c_void,
                        Some(call_filter::<F, C>),
                        Some(call_conflict::<F, C>),
                        context,
                    )
                } else {
                    ffi::sqlite3changeset_apply_strm(
                        db,
                        Some(x_input),
                        input_ref as *const &mut dyn Read as *mut c_void,
                        None,
                        Some(call_conflict::<F, C>),
                        context,
                    )
                }
            })
        })
    }

    /// Apply a changeset stream with SQLite's experimental v2 flags.
    ///
    /// Any rebase output is discarded.
    pub fn apply_strm_with_flags<F, C>(
        &self,
        input: &mut dyn Read,
        filter: Option<F>,
        conflict: C,
        flags: ChangesetApplyFlags,
    ) -> Result<()>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        self.apply_strm_with_flags_impl(input, filter, conflict, flags, false)?;
        Ok(())
    }

    /// Apply a changeset stream with SQLite's experimental v2 flags and
    /// return any rebase data generated when conflicts are resolved.
    ///
    /// The rebase data can be passed to [`Rebaser::configure`].
    pub fn apply_strm_with_flags_and_rebase<F, C>(
        &self,
        input: &mut dyn Read,
        filter: Option<F>,
        conflict: C,
        flags: ChangesetApplyFlags,
    ) -> Result<Option<Vec<u8>>>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        self.apply_strm_with_flags_impl(input, filter, conflict, flags, true)
    }

    fn apply_strm_with_flags_impl<F, C>(
        &self,
        input: &mut dyn Read,
        filter: Option<F>,
        conflict: C,
        flags: ChangesetApplyFlags,
        rebase: bool,
    ) -> Result<Option<Vec<u8>>>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        let input_ref = &input;
        let db = self.db.borrow_mut().db;
        let mut output = RebaseBuffer {
            ptr: ptr::null_mut(),
            len: 0,
        };
        self.with_apply_callbacks(
            filter,
            conflict,
            !flags.contains(ChangesetApplyFlags::NOSAVEPOINT),
            |filtered, context| {
                check(unsafe {
                    ffi::sqlite3changeset_apply_v2_strm(
                        db,
                        Some(x_input),
                        input_ref as *const &mut dyn Read as *mut c_void,
                        if filtered {
                            Some(call_filter::<F, C>)
                        } else {
                            None
                        },
                        Some(call_conflict::<F, C>),
                        context,
                        if rebase {
                            &mut output.ptr
                        } else {
                            ptr::null_mut()
                        },
                        if rebase {
                            &mut output.len
                        } else {
                            ptr::null_mut()
                        },
                        flags.bits(),
                    )
                })
            },
        )?;
        Ok(output.to_vec())
    }

    fn with_apply_callbacks<F, C, T>(
        &self,
        filter: Option<F>,
        conflict: C,
        use_savepoint: bool,
        apply: impl FnOnce(bool, *mut c_void) -> Result<T>,
    ) -> Result<T>
    where
        F: FnMut(&str) -> bool,
        C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
    {
        let filtered = filter.is_some();
        if filtered && use_savepoint {
            self.execute_batch("SAVEPOINT _rusqlite_changeset_filter")?;
        }
        let mut callbacks = ApplyCallbacks {
            filter,
            conflict,
            filter_panicked: false,
        };
        let result = apply(
            filtered,
            (&mut callbacks as *mut ApplyCallbacks<F, C>).cast(),
        );
        if filtered && use_savepoint {
            if callbacks.filter_panicked {
                self.execute_batch(
                    "ROLLBACK TO _rusqlite_changeset_filter; RELEASE _rusqlite_changeset_filter",
                )?;
                return Err(error_from_sqlite_code(ffi::SQLITE_ABORT, None));
            }
            self.execute_batch("RELEASE _rusqlite_changeset_filter")?;
        }
        if callbacks.filter_panicked {
            return Err(error_from_sqlite_code(ffi::SQLITE_ABORT, None));
        }
        result
    }
}

struct ApplyCallbacks<F, C> {
    filter: Option<F>,
    conflict: C,
    filter_panicked: bool,
}

/// Constants passed to the conflict handler
/// See [here](https://sqlite.org/session.html#SQLITE_CHANGESET_CONFLICT) for details.
#[allow(missing_docs)]
#[repr(i32)]
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictType {
    UNKNOWN = -1,
    SQLITE_CHANGESET_DATA = ffi::SQLITE_CHANGESET_DATA,
    SQLITE_CHANGESET_NOTFOUND = ffi::SQLITE_CHANGESET_NOTFOUND,
    SQLITE_CHANGESET_CONFLICT = ffi::SQLITE_CHANGESET_CONFLICT,
    SQLITE_CHANGESET_CONSTRAINT = ffi::SQLITE_CHANGESET_CONSTRAINT,
    SQLITE_CHANGESET_FOREIGN_KEY = ffi::SQLITE_CHANGESET_FOREIGN_KEY,
}
impl From<i32> for ConflictType {
    fn from(code: i32) -> ConflictType {
        match code {
            ffi::SQLITE_CHANGESET_DATA => ConflictType::SQLITE_CHANGESET_DATA,
            ffi::SQLITE_CHANGESET_NOTFOUND => ConflictType::SQLITE_CHANGESET_NOTFOUND,
            ffi::SQLITE_CHANGESET_CONFLICT => ConflictType::SQLITE_CHANGESET_CONFLICT,
            ffi::SQLITE_CHANGESET_CONSTRAINT => ConflictType::SQLITE_CHANGESET_CONSTRAINT,
            ffi::SQLITE_CHANGESET_FOREIGN_KEY => ConflictType::SQLITE_CHANGESET_FOREIGN_KEY,
            _ => ConflictType::UNKNOWN,
        }
    }
}

/// Constants returned by the conflict handler
/// See [here](https://sqlite.org/session.html#SQLITE_CHANGESET_ABORT) for details.
#[allow(missing_docs)]
#[repr(i32)]
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConflictAction {
    SQLITE_CHANGESET_OMIT = ffi::SQLITE_CHANGESET_OMIT,
    SQLITE_CHANGESET_REPLACE = ffi::SQLITE_CHANGESET_REPLACE,
    SQLITE_CHANGESET_ABORT = ffi::SQLITE_CHANGESET_ABORT,
}

unsafe extern "C" fn call_filter<F, C>(p_ctx: *mut c_void, tbl_str: *const c_char) -> c_int
where
    F: FnMut(&str) -> bool,
    C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
{
    unsafe {
        let tbl_name = CStr::from_ptr(tbl_str).to_str();
        c_int::from(
            catch_unwind(|| {
                let callbacks = &mut *p_ctx.cast::<ApplyCallbacks<F, C>>();
                if let Some(ref mut filter) = callbacks.filter {
                    filter(tbl_name.expect("illegal table name"))
                } else {
                    true
                }
            })
            .unwrap_or_else(|_| {
                (*p_ctx.cast::<ApplyCallbacks<F, C>>()).filter_panicked = true;
                false
            }),
        )
    }
}

unsafe extern "C" fn call_conflict<F, C>(
    p_ctx: *mut c_void,
    e_conflict: c_int,
    p: *mut ffi::sqlite3_changeset_iter,
) -> c_int
where
    F: FnMut(&str) -> bool,
    C: for<'a> FnMut(ConflictType, ChangesetItem<'a>) -> ConflictAction,
{
    let conflict_type = ConflictType::from(e_conflict);
    let item = ChangesetItem {
        it: p,
        phantom: PhantomData,
    };
    unsafe {
        if let Ok(action) = catch_unwind(|| {
            let callbacks = &mut *p_ctx.cast::<ApplyCallbacks<F, C>>();
            (callbacks.conflict)(conflict_type, item)
        }) {
            action as c_int
        } else {
            ffi::SQLITE_CHANGESET_ABORT
        }
    }
}

unsafe extern "C" fn x_input(p_in: *mut c_void, data: *mut c_void, len: *mut c_int) -> c_int {
    if p_in.is_null() {
        return ffi::SQLITE_MISUSE;
    }
    unsafe {
        let bytes: &mut [u8] = from_raw_parts_mut(data as *mut u8, *len as usize);
        let input = p_in as *mut &mut dyn Read;
        match catch_unwind(std::panic::AssertUnwindSafe(|| (*input).read(bytes))) {
            Ok(Ok(n)) => {
                *len = n as i32; // TODO Validate: n = 0 may not mean the reader will always no longer be able to
                // produce bytes.
                ffi::SQLITE_OK
            }
            Ok(Err(_)) | Err(_) => ffi::SQLITE_IOERR_READ,
        }
    }
}

unsafe extern "C" fn x_output(p_out: *mut c_void, data: *const c_void, len: c_int) -> c_int {
    if p_out.is_null() {
        return ffi::SQLITE_MISUSE;
    }
    unsafe {
        // The sessions module never invokes an xOutput callback with the third
        // parameter set to a value less than or equal to zero.
        let bytes: &[u8] = from_raw_parts(data as *const u8, len as usize);
        let output = p_out as *mut &mut dyn Write;
        match catch_unwind(std::panic::AssertUnwindSafe(|| (*output).write_all(bytes))) {
            Ok(Ok(())) => ffi::SQLITE_OK,
            Ok(Err(_)) | Err(_) => ffi::SQLITE_IOERR_WRITE,
        }
    }
}

#[cfg(all(test, not(miri)))]
mod test {
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use std::io::{self, Read};
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{
        Changeset, ChangesetApplyFlags, ChangesetIter, ChangesetOperation, ConflictAction,
        ConflictType, Rebaser, Session,
    };
    use crate::hooks::Action;
    use crate::{Connection, Result};

    fn one_changeset_insert() -> Result<Changeset> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        let mut session = Session::new(&db)?;
        assert!(session.is_empty());

        session.attach::<&str>(None)?;
        db.execute("INSERT INTO foo (t) VALUES (?1);", ["bar"])?;

        session.changeset()
    }

    fn one_changeset_update() -> Result<Changeset> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL, i INTEGER NOT NULL DEFAULT 0);",
        )?;
        db.execute_batch("INSERT INTO foo (t) VALUES ('bar');")?;

        let mut session = Session::new(&db)?;
        session.attach::<&str>(None)?;
        db.execute("UPDATE foo SET i=100 WHERE t='bar';", [])?;

        session.changeset()
    }

    fn one_changeset_strm() -> Result<Vec<u8>> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        let mut session = Session::new(&db)?;
        assert!(session.is_empty());

        session.attach::<&str>(None)?;
        db.execute("INSERT INTO foo (t) VALUES (?1);", ["bar"])?;

        let mut output = Vec::new();
        session.changeset_strm(&mut output)?;
        Ok(output)
    }

    #[test]
    fn test_changeset() -> Result<()> {
        let changeset = one_changeset_insert()?;
        let mut iter = changeset.iter()?;
        let item = iter.next()?;
        assert!(item.is_some());

        let item = item.unwrap();
        let op = item.op()?;
        assert_eq!("foo", op.table_name());
        assert_eq!(1, op.number_of_columns());
        assert_eq!(Action::SQLITE_INSERT, op.code());
        assert_eq!(ChangesetOperation::Insert, op.changeset_operation()?);
        assert_eq!(1, op.column_count()?);
        assert!(!op.indirect());

        let pk = item.pk()?;
        assert_eq!(&[1], pk);

        let new_value = item.new_value(0)?;
        assert_eq!(Ok("bar"), new_value.as_str());
        Ok(())
    }

    #[test]
    fn test_changeset_strm() -> Result<()> {
        let output = one_changeset_strm()?;
        assert!(!output.is_empty());
        assert_eq!(14, output.len());

        let mut input = output.as_slice();
        let mut iter = ChangesetIter::start_strm(&mut input)?;
        let item = iter.next()?;
        assert!(item.is_some());
        Ok(())
    }

    #[test]
    fn test_changeset_strm_context_survives_move() -> Result<()> {
        let output = one_changeset_strm()?;
        let mut input = output.as_slice();
        let mut iter = Box::new(ChangesetIter::start_strm(&mut input)?);
        let item = iter.next()?.unwrap();
        assert_eq!(item.new_value_opt(0)?.unwrap().as_str(), Ok("bar"));
        assert!(iter.next()?.is_none());
        Ok(())
    }

    #[test]
    fn test_changeset_strm_reader_panic() -> Result<()> {
        struct PanickingReader;
        impl Read for PanickingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                panic!("reader panic");
            }
        }

        let mut reader = PanickingReader;
        if let Ok(mut iter) = ChangesetIter::start_strm(&mut reader) {
            assert!(iter.next().is_err());
        }
        Ok(())
    }

    #[test]
    fn test_changeset_values() -> Result<()> {
        let changeset = one_changeset_update()?;
        let mut iter = changeset.iter()?;
        let item = iter.next()?.unwrap();

        let new_value = item.new_value(0); // unchanged
        assert_eq!(Err(crate::Error::InvalidColumnIndex(0)), new_value);
        let new_value = item.new_value(1)?; // updated
        assert_eq!(Ok(100), new_value.as_i64());
        assert!(item.new_value_opt(0)?.is_none());
        assert_eq!(item.new_value_opt(1)?.unwrap().as_i64(), Ok(100));
        assert_eq!(item.old_value_opt(0)?.unwrap().as_str(), Ok("bar"));
        assert_eq!(item.old_value_opt(1)?.unwrap().as_i64(), Ok(0));
        Ok(())
    }

    #[test]
    fn test_changeset_borrowed_bytes() -> Result<()> {
        let bytes = one_changeset_strm()?;
        let mut iterator = ChangesetIter::start_bytes(&bytes)?;
        let item = iterator.next()?.unwrap();
        assert_eq!(item.new_value_opt(0)?.unwrap().as_str(), Ok("bar"));
        assert!(item.old_value_opt(0).is_err());

        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;
        db.apply_bytes_with_flags(
            &bytes,
            None::<fn(&str) -> bool>,
            |_kind, _item| ConflictAction::SQLITE_CHANGESET_ABORT,
            ChangesetApplyFlags::empty(),
        )?;
        assert_eq!(
            db.query_row("SELECT t FROM foo", [], |row| row.get::<_, String>(0))?,
            "bar"
        );
        Ok(())
    }

    #[test]
    fn test_changeset_apply() -> Result<()> {
        let changeset = one_changeset_insert()?;

        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        static CALLED: AtomicBool = AtomicBool::new(false);
        db.apply(
            &changeset,
            None::<fn(&str) -> bool>,
            |_conflict_type, _item| {
                CALLED.store(true, Ordering::Relaxed);
                ConflictAction::SQLITE_CHANGESET_OMIT
            },
        )?;

        assert!(!CALLED.load(Ordering::Relaxed));
        let check = db.query_row("SELECT 1 FROM foo WHERE t = ?1", ["bar"], |row| {
            row.get::<_, i32>(0)
        })?;
        assert_eq!(1, check);

        // conflict expected when same changeset applied again on the same db
        db.apply(
            &changeset,
            None::<fn(&str) -> bool>,
            |conflict_type, item| {
                CALLED.store(true, Ordering::Relaxed);
                assert_eq!(ConflictType::SQLITE_CHANGESET_CONFLICT, conflict_type);
                let conflict = item.conflict(0).unwrap();
                assert_eq!(Ok("bar"), conflict.as_str());
                ConflictAction::SQLITE_CHANGESET_OMIT
            },
        )?;
        assert!(CALLED.load(Ordering::Relaxed));
        Ok(())
    }

    #[test]
    fn test_changeset_filter_panic_rolls_back() -> Result<()> {
        let source = Connection::open_in_memory()?;
        source.execute_batch(
            "CREATE TABLE foo(t TEXT PRIMARY KEY);
             CREATE TABLE bar(t TEXT PRIMARY KEY);",
        )?;
        let mut session = Session::new(&source)?;
        session.attach(Some("foo"))?;
        session.attach(Some("bar"))?;
        source
            .execute_batch("INSERT INTO foo VALUES ('first'); INSERT INTO bar VALUES ('second')")?;
        let changeset = session.changeset()?;

        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE foo(t TEXT PRIMARY KEY);
             CREATE TABLE bar(t TEXT PRIMARY KEY);",
        )?;
        let result = db.apply(
            &changeset,
            Some(|table: &str| {
                if table == "bar" {
                    panic!("filter panic");
                }
                true
            }),
            |_kind, _item| ConflictAction::SQLITE_CHANGESET_ABORT,
        );
        assert!(result.is_err());
        assert_eq!(
            db.query_row("SELECT count(*) FROM foo", [], |row| row.get::<_, i64>(0))?,
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM bar", [], |row| row.get::<_, i64>(0))?,
            0
        );
        Ok(())
    }

    #[test]
    fn test_changeset_filter_panic_v2() -> Result<()> {
        let changeset = one_changeset_insert()?;
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        assert!(
            db.apply_with_flags(
                &changeset,
                Some(|_: &str| panic!("filter panic")),
                |_kind, _item| ConflictAction::SQLITE_CHANGESET_ABORT,
                ChangesetApplyFlags::empty(),
            )
            .is_err()
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM foo", [], |row| row.get::<_, i64>(0))?,
            0
        );
        assert!(
            db.apply_with_flags(
                &changeset,
                Some(|_: &str| panic!("filter panic")),
                |_kind, _item| ConflictAction::SQLITE_CHANGESET_ABORT,
                ChangesetApplyFlags::NOSAVEPOINT,
            )
            .is_err()
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM foo", [], |row| row.get::<_, i64>(0))?,
            0
        );
        Ok(())
    }

    #[test]
    fn test_changeset_apply_strm() -> Result<()> {
        let output = one_changeset_strm()?;

        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        let mut input = output.as_slice();
        db.apply_strm(
            &mut input,
            None::<fn(&str) -> bool>,
            |_conflict_type, _item| ConflictAction::SQLITE_CHANGESET_OMIT,
        )?;

        let check = db.query_row("SELECT 1 FROM foo WHERE t = ?1", ["bar"], |row| {
            row.get::<_, i32>(0)
        })?;
        assert_eq!(1, check);
        Ok(())
    }

    #[test]
    fn test_changeset_borrowed_callbacks() -> Result<()> {
        let changeset = one_changeset_insert()?;
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);
             INSERT INTO foo(t) VALUES ('bar');",
        )?;
        let mut filtered = 0;
        let mut conflicts = 0;
        db.apply(
            &changeset,
            Some(|_: &str| {
                filtered += 1;
                true
            }),
            |kind, _item| {
                assert_eq!(kind, ConflictType::SQLITE_CHANGESET_CONFLICT);
                conflicts += 1;
                ConflictAction::SQLITE_CHANGESET_OMIT
            },
        )?;
        assert_eq!(filtered, 1);
        assert_eq!(conflicts, 1);
        Ok(())
    }

    #[test]
    fn test_changeset_callback_panic() -> Result<()> {
        let changeset = one_changeset_insert()?;
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);
             INSERT INTO foo(t) VALUES ('bar');",
        )?;
        assert!(
            db.apply(&changeset, None::<fn(&str) -> bool>, |_kind, _item| panic!(
                "callback panic"
            ),)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn test_changeset_apply_with_flags() -> Result<()> {
        let changeset = one_changeset_insert()?;
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        db.apply_with_flags(
            &changeset,
            Some(|table: &str| table == "foo"),
            |_kind, _item| ConflictAction::SQLITE_CHANGESET_ABORT,
            ChangesetApplyFlags::empty(),
        )?;
        assert_eq!(
            db.query_row("SELECT count(*) FROM foo", [], |row| row.get::<_, i32>(0))?,
            1
        );
        Ok(())
    }

    #[test]
    fn test_changeset_rebase() -> Result<()> {
        let remote = one_changeset_update()?;
        let local = Connection::open_in_memory()?;
        local.execute_batch(
            "CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL, i INTEGER NOT NULL DEFAULT 0);
             INSERT INTO foo(t) VALUES ('bar');",
        )?;
        let mut session = Session::new(&local)?;
        session.attach(Some("foo"))?;
        local.execute("UPDATE foo SET i = 200 WHERE t = 'bar'", [])?;
        let local_changeset = session.changeset()?;
        let mut local_stream = Vec::new();
        session.changeset_strm(&mut local_stream)?;

        let rebase = local
            .apply_with_flags_and_rebase(
                &remote,
                None::<fn(&str) -> bool>,
                |kind, _item| {
                    assert_eq!(kind, ConflictType::SQLITE_CHANGESET_DATA);
                    ConflictAction::SQLITE_CHANGESET_OMIT
                },
                ChangesetApplyFlags::empty(),
            )?
            .expect("conflict produces rebase data");
        assert!(!rebase.is_empty());
        let mut rebaser = Rebaser::new()?;
        rebaser.configure(&rebase)?;
        let rebased = rebaser.rebase(&local_changeset)?;
        let mut rebased_stream = Vec::new();
        rebaser.rebase_strm(&mut local_stream.as_slice(), &mut rebased_stream)?;

        let remote_db = Connection::open_in_memory()?;
        remote_db.execute_batch(
            "CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL, i INTEGER NOT NULL DEFAULT 0);
             INSERT INTO foo(t) VALUES ('bar');",
        )?;
        remote_db.apply(&remote, None::<fn(&str) -> bool>, |_kind, _item| {
            ConflictAction::SQLITE_CHANGESET_ABORT
        })?;
        remote_db.apply(&rebased, None::<fn(&str) -> bool>, |_kind, _item| {
            ConflictAction::SQLITE_CHANGESET_ABORT
        })?;
        assert_eq!(
            remote_db.query_row("SELECT i FROM foo WHERE t = 'bar'", [], |row| row
                .get::<_, i32>(0))?,
            200
        );

        let streamed_db = Connection::open_in_memory()?;
        streamed_db.execute_batch(
            "CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL, i INTEGER NOT NULL DEFAULT 0);
             INSERT INTO foo(t) VALUES ('bar');",
        )?;
        streamed_db.apply(&remote, None::<fn(&str) -> bool>, |_kind, _item| {
            ConflictAction::SQLITE_CHANGESET_ABORT
        })?;
        streamed_db.apply_strm(
            &mut rebased_stream.as_slice(),
            None::<fn(&str) -> bool>,
            |_kind, _item| ConflictAction::SQLITE_CHANGESET_ABORT,
        )?;
        assert_eq!(
            streamed_db.query_row("SELECT i FROM foo WHERE t = 'bar'", [], |row| row
                .get::<_, i32>(0))?,
            200
        );
        Ok(())
    }

    #[test]
    fn test_changeset_size() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;
        let mut session = Session::new(&db)?;
        assert_eq!(session.changeset_size(), 0);
        session.set_changeset_size_tracking(true)?;
        session.attach(Some("foo"))?;
        db.execute("INSERT INTO foo(t) VALUES ('bar')", [])?;
        let changeset = session.changeset()?;
        assert!(session.changeset_size() >= i64::from(changeset.n));
        assert!(session.set_changeset_size_tracking(false).is_err());
        Ok(())
    }

    #[test]
    fn test_changeset_apply_strm_rebase() -> Result<()> {
        let changeset = one_changeset_strm()?;
        let clean_db = Connection::open_in_memory()?;
        clean_db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;
        assert!(
            clean_db
                .apply_strm_with_flags_and_rebase(
                    &mut changeset.as_slice(),
                    None::<fn(&str) -> bool>,
                    |_kind, _item| ConflictAction::SQLITE_CHANGESET_ABORT,
                    ChangesetApplyFlags::empty(),
                )?
                .is_none()
        );

        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);
             INSERT INTO foo(t) VALUES ('bar');",
        )?;
        assert!(
            db.apply_strm_with_flags_and_rebase(
                &mut changeset.as_slice(),
                None::<fn(&str) -> bool>,
                |_kind, _item| ConflictAction::SQLITE_CHANGESET_ABORT,
                ChangesetApplyFlags::empty(),
            )
            .is_err()
        );
        let rebase = db.apply_strm_with_flags_and_rebase(
            &mut changeset.as_slice(),
            None::<fn(&str) -> bool>,
            |kind, _item| {
                assert_eq!(kind, ConflictType::SQLITE_CHANGESET_CONFLICT);
                ConflictAction::SQLITE_CHANGESET_OMIT
            },
            ChangesetApplyFlags::empty(),
        )?;
        assert!(rebase.is_some_and(|bytes| !bytes.is_empty()));
        Ok(())
    }

    #[test]
    fn test_changeset_apply_strm_fknoaction() -> Result<()> {
        let source = Connection::open_in_memory()?;
        source.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE parent(id INTEGER PRIMARY KEY);
             CREATE TABLE child(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE);
             INSERT INTO parent VALUES (1);
             INSERT INTO child VALUES (1, 1);",
        )?;
        let mut session = Session::new(&source)?;
        session.attach(Some("parent"))?;
        source.execute("DELETE FROM parent WHERE id = 1", [])?;
        let mut changeset = Vec::new();
        session.changeset_strm(&mut changeset)?;

        let target = Connection::open_in_memory()?;
        target.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE parent(id INTEGER PRIMARY KEY);
             CREATE TABLE child(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE);
             INSERT INTO parent VALUES (1);
             INSERT INTO child VALUES (1, 1);",
        )?;
        let mut input = changeset.as_slice();
        let foreign_key_conflict = std::sync::Arc::new(AtomicBool::new(false));
        let callback_conflict = foreign_key_conflict.clone();
        assert!(
            target
                .apply_strm_with_flags(
                    &mut input,
                    None::<fn(&str) -> bool>,
                    move |kind, _item| {
                        callback_conflict.store(
                            kind == ConflictType::SQLITE_CHANGESET_FOREIGN_KEY,
                            Ordering::Relaxed,
                        );
                        ConflictAction::SQLITE_CHANGESET_ABORT
                    },
                    ChangesetApplyFlags::FKNOACTION,
                )
                .is_err()
        );
        assert!(foreign_key_conflict.load(Ordering::Relaxed));
        assert_eq!(
            target.query_row("SELECT count(*) FROM parent", [], |row| row
                .get::<_, i32>(0))?,
            1
        );
        assert_eq!(
            target.query_row("SELECT count(*) FROM child", [], |row| row.get::<_, i32>(0))?,
            1
        );

        let mut input = changeset.as_slice();
        target.apply_strm(&mut input, None::<fn(&str) -> bool>, |_kind, _item| {
            ConflictAction::SQLITE_CHANGESET_ABORT
        })?;
        assert_eq!(
            target.query_row("SELECT count(*) FROM child", [], |row| row.get::<_, i32>(0))?,
            0
        );
        Ok(())
    }

    #[test]
    fn test_session_empty() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo(t TEXT PRIMARY KEY NOT NULL);")?;

        let mut session = Session::new(&db)?;
        assert!(session.is_empty());

        session.attach::<&str>(None)?;
        db.execute("INSERT INTO foo (t) VALUES (?1);", ["bar"])?;

        assert!(!session.is_empty());
        Ok(())
    }

    #[test]
    fn test_session_set_enabled() -> Result<()> {
        let db = Connection::open_in_memory()?;

        let mut session = Session::new(&db)?;
        assert!(session.is_enabled());
        session.set_enabled(false);
        assert!(!session.is_enabled());
        Ok(())
    }

    #[test]
    fn test_session_set_indirect() -> Result<()> {
        let db = Connection::open_in_memory()?;

        let mut session = Session::new(&db)?;
        assert!(!session.is_indirect());
        session.set_indirect(true);
        assert!(session.is_indirect());
        Ok(())
    }
}
