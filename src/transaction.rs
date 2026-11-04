use crate::pragma::Sql;
use crate::{Connection, Error, Result};
use std::error;
use std::fmt;
use std::ops::Deref;

/// Options for transaction behavior. See [BEGIN
/// TRANSACTION](http://www.sqlite.org/lang_transaction.html) for details.
#[derive(Copy, Clone)]
#[non_exhaustive]
pub enum TransactionBehavior {
    /// DEFERRED means that the transaction does not actually start until the
    /// database is first accessed.
    Deferred,
    /// IMMEDIATE cause the database connection to start a new write
    /// immediately, without waiting for a writes statement.
    Immediate,
    /// EXCLUSIVE prevents other database connections from reading the database
    /// while the transaction is underway.
    Exclusive,
}

/// Options for how a Transaction or Savepoint should behave when it is dropped.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropBehavior {
    /// Roll back the changes. This is the default.
    Rollback,

    /// Commit the changes.
    Commit,

    /// Do not commit or roll back changes - this will leave the transaction or
    /// savepoint open, so should be used with care.
    Ignore,

    /// Panic. Used to enforce intentional behavior during development.
    Panic,
}

/// Represents a transaction on a database connection.
///
/// ## Note
///
/// Transactions will roll back by default. Use `commit` method to explicitly
/// commit the transaction, or use `set_drop_behavior` to change what happens
/// when the transaction is dropped.
///
/// ## Example
///
/// ```rust,no_run
/// # use rusqlite::{Connection, Result};
/// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
/// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
/// fn perform_queries(conn: &mut Connection) -> Result<()> {
///     let tx = conn.transaction()?;
///
///     do_queries_part_1(&tx)?; // tx causes rollback if this fails
///     do_queries_part_2(&tx)?; // tx causes rollback if this fails
///
///     tx.commit()
/// }
/// ```
#[derive(Debug)]
pub struct Transaction<'conn> {
    conn: &'conn Connection,
    drop_behavior: DropBehavior,
}

/// Represents a savepoint on a database connection.
///
/// ## Note
///
/// Savepoints will roll back by default. Use `commit` method to explicitly
/// commit the savepoint, or use `set_drop_behavior` to change what happens
/// when the savepoint is dropped.
///
/// ## Example
///
/// ```rust,no_run
/// # use rusqlite::{Connection, Result};
/// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
/// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
/// fn perform_queries(conn: &mut Connection) -> Result<()> {
///     let sp = conn.savepoint()?;
///
///     do_queries_part_1(&sp)?; // sp causes rollback if this fails
///     do_queries_part_2(&sp)?; // sp causes rollback if this fails
///
///     sp.commit()
/// }
/// ```
#[derive(Debug)]
pub struct Savepoint<'conn> {
    conn: &'conn Connection,
    name: String,
    drop_behavior: DropBehavior,
    committed: bool,
}

/// The operation that failed inside a scoped transaction or savepoint.
#[derive(Debug)]
pub enum TransactionFailure<E> {
    /// Opening the transaction or savepoint failed.
    Begin(Error),
    /// The callback returned its own error.
    Operation(E),
    /// Committing the transaction or releasing its savepoint failed.
    Commit(Error),
    /// The callback succeeded, but rollback or savepoint release failed.
    Cleanup,
}

/// A scoped transaction or savepoint failure together with cleanup failures.
///
/// Callback errors have no trait bounds. This wrapper implements
/// [`std::error::Error`] when `E` does. Its public fields also allow callers to
/// handle other error containers without converting their original values.
#[derive(Debug)]
pub struct TransactionError<E> {
    /// The original failure.
    pub cause: TransactionFailure<E>,
    /// Failure while rolling back changes after the original failure.
    pub rollback_error: Option<Box<Error>>,
    /// Whether cleanup attempted a rollback statement.
    pub rollback_attempted: bool,
    /// Failure while releasing a rolled back savepoint, unused for transactions.
    pub release_error: Option<Box<Error>>,
    /// Whether the connection is in autocommit mode after cleanup.
    pub is_autocommit: bool,
}
impl<E: fmt::Display> fmt::Display for TransactionError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cause {
            TransactionFailure::Begin(error) => write!(f, "Could not begin transaction: {error}"),
            TransactionFailure::Operation(error) => {
                write!(f, "Transaction operation failed: {error}")
            }
            TransactionFailure::Commit(error) => write!(f, "Could not commit transaction: {error}"),
            TransactionFailure::Cleanup => write!(f, "Could not finish rollback scope"),
        }?;

        if let Some(error) = &self.rollback_error {
            write!(f, ", rollback failed: {error}")?;
        }
        if let Some(error) = &self.release_error {
            write!(f, ", release failed: {error}")?;
        }

        Ok(())
    }
}
impl<E: error::Error + 'static> error::Error for TransactionError<E> {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match &self.cause {
            TransactionFailure::Begin(error) | TransactionFailure::Commit(error) => Some(error),
            TransactionFailure::Operation(error) => Some(error),
            TransactionFailure::Cleanup => self
                .rollback_error
                .as_deref()
                .or(self.release_error.as_deref())
                .map(|error| error as _),
        }
    }
}

impl Transaction<'_> {
    /// Begin a new transaction. Cannot be nested; see `savepoint` for nested
    /// transactions.
    ///
    /// Even though we don't mutate the connection, we take a `&mut Connection`
    /// to prevent nested transactions on the same connection. For cases
    /// where this is unacceptable, [`Transaction::new_unchecked`] is available.
    #[inline]
    pub fn new(conn: &mut Connection, behavior: TransactionBehavior) -> Result<Transaction<'_>> {
        Self::new_unchecked(conn, behavior)
    }

    /// Begin a new transaction, failing if a transaction is open.
    ///
    /// If a transaction is already open, this will return an error. Where
    /// possible, [`Transaction::new`] should be preferred, as it provides a
    /// compile-time guarantee that transactions are not nested.
    #[inline]
    pub fn new_unchecked(
        conn: &Connection,
        behavior: TransactionBehavior,
    ) -> Result<Transaction<'_>> {
        let query = match behavior {
            TransactionBehavior::Deferred => "BEGIN DEFERRED",
            TransactionBehavior::Immediate => "BEGIN IMMEDIATE",
            TransactionBehavior::Exclusive => "BEGIN EXCLUSIVE",
        };
        conn.execute_batch(query).map(move |()| Transaction {
            conn,
            drop_behavior: DropBehavior::Rollback,
        })
    }

    /// Starts a new [savepoint](http://www.sqlite.org/lang_savepoint.html), allowing nested
    /// transactions.
    ///
    /// ## Note
    ///
    /// Just like outer level transactions, savepoint transactions rollback by
    /// default.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// # fn perform_queries_part_1_succeeds(_conn: &Connection) -> bool { true }
    /// fn perform_queries(conn: &mut Connection) -> Result<()> {
    ///     let mut tx = conn.transaction()?;
    ///
    ///     {
    ///         let sp = tx.savepoint()?;
    ///         if perform_queries_part_1_succeeds(&sp) {
    ///             sp.commit()?;
    ///         }
    ///         // otherwise, sp will rollback
    ///     }
    ///
    ///     tx.commit()
    /// }
    /// ```
    #[inline]
    pub fn savepoint(&mut self) -> Result<Savepoint<'_>> {
        Savepoint::new_(self.conn)
    }

    /// Create a new savepoint with a custom savepoint name. See `savepoint()`.
    #[inline]
    pub fn savepoint_with_name<T: Into<String>>(&mut self, name: T) -> Result<Savepoint<'_>> {
        Savepoint::with_name_(self.conn, name)
    }

    /// Get the current setting for what happens to the transaction when it is
    /// dropped.
    #[inline]
    #[must_use]
    pub fn drop_behavior(&self) -> DropBehavior {
        self.drop_behavior
    }

    /// Configure the transaction to perform the specified action when it is
    /// dropped.
    #[inline]
    pub fn set_drop_behavior(&mut self, drop_behavior: DropBehavior) {
        self.drop_behavior = drop_behavior;
    }

    /// A convenience method which consumes and commits a transaction.
    #[inline]
    pub fn commit(mut self) -> Result<()> {
        self.commit_()
    }

    #[inline]
    fn commit_(&mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// A convenience method which consumes and rolls back a transaction.
    #[inline]
    pub fn rollback(mut self) -> Result<()> {
        self.rollback_()
    }

    #[inline]
    fn rollback_(&mut self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    /// Consumes the transaction, committing or rolling back according to the
    /// current setting (see `drop_behavior`).
    ///
    /// Functionally equivalent to the `Drop` implementation, but allows
    /// callers to see any errors that occur.
    #[inline]
    pub fn finish(mut self) -> Result<()> {
        let result = self.finish_();
        self.drop_behavior = DropBehavior::Ignore;
        result
    }

    #[inline]
    fn finish_(&mut self) -> Result<()> {
        if self.conn.is_autocommit() {
            return Ok(());
        }
        match self.drop_behavior() {
            DropBehavior::Commit => {
                let result = self.commit_();
                if result.is_err() {
                    let _ = self.rollback_();
                }

                result
            }
            DropBehavior::Rollback => self.rollback_(),
            DropBehavior::Ignore => Ok(()),
            DropBehavior::Panic => panic!("Transaction dropped unexpectedly."),
        }
    }
}

impl Deref for Transaction<'_> {
    type Target = Connection;

    #[inline]
    fn deref(&self) -> &Connection {
        self.conn
    }
}

#[expect(unused_must_use)]
impl Drop for Transaction<'_> {
    #[inline]
    fn drop(&mut self) {
        self.finish_();
    }
}

impl Savepoint<'_> {
    #[inline]
    fn with_name_<T: Into<String>>(conn: &Connection, name: T) -> Result<Savepoint<'_>> {
        let name = name.into();
        let sql = cmd("SAVEPOINT", false, name.as_str())?;
        conn.execute_batch(sql.as_str()).map(|()| Savepoint {
            conn,
            name,
            drop_behavior: DropBehavior::Rollback,
            committed: false,
        })
    }

    #[inline]
    fn new_(conn: &Connection) -> Result<Savepoint<'_>> {
        Savepoint::with_name_(conn, "_rusqlite_sp")
    }

    /// Begin a new savepoint. Can be nested.
    #[inline]
    pub fn new(conn: &mut Connection) -> Result<Savepoint<'_>> {
        Savepoint::new_(conn)
    }

    /// Begin a savepoint using a shared connection reference.
    ///
    /// Unlike [`Savepoint::new`], this cannot prevent overlapping savepoints
    /// at compile time. Use the mutable variant when possible.
    #[inline]
    pub fn new_unchecked(conn: &Connection) -> Result<Savepoint<'_>> {
        Savepoint::new_(conn)
    }

    /// Begin a new savepoint with a user-provided savepoint name.
    #[inline]
    pub fn with_name<T: Into<String>>(conn: &mut Connection, name: T) -> Result<Savepoint<'_>> {
        Savepoint::with_name_(conn, name)
    }

    /// Begin a named savepoint using a shared connection reference.
    ///
    /// Unlike [`Savepoint::with_name`], this cannot prevent overlapping
    /// savepoints at compile time. Use the mutable variant when possible.
    #[inline]
    pub fn with_name_unchecked<T: Into<String>>(
        conn: &Connection,
        name: T,
    ) -> Result<Savepoint<'_>> {
        Savepoint::with_name_(conn, name)
    }

    /// Begin a nested savepoint.
    #[inline]
    pub fn savepoint(&mut self) -> Result<Savepoint<'_>> {
        Savepoint::new_(self.conn)
    }

    /// Begin a nested savepoint with a user-provided savepoint name.
    #[inline]
    pub fn savepoint_with_name<T: Into<String>>(&mut self, name: T) -> Result<Savepoint<'_>> {
        Savepoint::with_name_(self.conn, name)
    }

    /// Get the current setting for what happens to the savepoint when it is
    /// dropped.
    #[inline]
    #[must_use]
    pub fn drop_behavior(&self) -> DropBehavior {
        self.drop_behavior
    }

    /// Configure the savepoint to perform the specified action when it is
    /// dropped.
    #[inline]
    pub fn set_drop_behavior(&mut self, drop_behavior: DropBehavior) {
        self.drop_behavior = drop_behavior;
    }

    /// A convenience method which consumes and commits a savepoint.
    #[inline]
    pub fn commit(mut self) -> Result<()> {
        self.commit_()
    }

    #[inline]
    fn commit_(&mut self) -> Result<()> {
        let sql = cmd("RELEASE", false, self.name.as_str())?;
        self.conn.execute_batch(sql.as_str())?;
        self.committed = true;
        Ok(())
    }

    /// A convenience method which rolls back a savepoint.
    ///
    /// ## Note
    ///
    /// Unlike `Transaction`s, savepoints remain active after they have been
    /// rolled back, and can be rolled back again or committed.
    #[inline]
    pub fn rollback(&mut self) -> Result<()> {
        let sql = cmd("ROLLBACK", true, self.name.as_str())?;
        self.conn.execute_batch(sql.as_str())
    }

    /// Consumes the savepoint, committing or rolling back according to the
    /// current setting (see `drop_behavior`).
    ///
    /// Functionally equivalent to the `Drop` implementation, but allows
    /// callers to see any errors that occur.
    #[inline]
    pub fn finish(mut self) -> Result<()> {
        let result = self.finish_();
        self.drop_behavior = DropBehavior::Ignore;
        result
    }

    #[inline]
    fn finish_(&mut self) -> Result<()> {
        if self.committed {
            return Ok(());
        }
        match self.drop_behavior() {
            DropBehavior::Commit => {
                let result = self.commit_();
                if result.is_err() {
                    let _ = self.rollback().and_then(|()| self.commit_());
                }

                result
            }
            DropBehavior::Rollback => self.rollback().and_then(|()| self.commit_()),
            DropBehavior::Ignore => Ok(()),
            DropBehavior::Panic => panic!("Savepoint dropped unexpectedly."),
        }
    }
}

fn cmd(cmd: &'static str, to: bool, name: &str) -> Result<Sql> {
    let mut sql = Sql::new();
    sql.push_keyword(cmd)?;
    sql.push_space();
    if to {
        sql.push_keyword("TO")?;
        sql.push_space();
    }
    sql.push_identifier(name);
    Ok(sql)
}

impl Deref for Savepoint<'_> {
    type Target = Connection;

    #[inline]
    fn deref(&self) -> &Connection {
        self.conn
    }
}

#[expect(unused_must_use)]
impl Drop for Savepoint<'_> {
    #[inline]
    fn drop(&mut self) {
        self.finish_();
    }
}

/// Transaction state of a database
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransactionState {
    /// Equivalent to `SQLITE_TXN_NONE`
    None,
    /// Equivalent to `SQLITE_TXN_READ`
    Read,
    /// Equivalent to `SQLITE_TXN_WRITE`
    Write,
}

impl Connection {
    /// Begin a new transaction with the default behavior (DEFERRED).
    ///
    /// The transaction defaults to rolling back when it is dropped. If you
    /// want the transaction to commit, you must call
    /// [`commit`](Transaction::commit) or
    /// [`set_drop_behavior(DropBehavior::Commit)`](Transaction::set_drop_behavior).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
    /// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
    /// fn perform_queries(conn: &mut Connection) -> Result<()> {
    ///     let tx = conn.transaction()?;
    ///
    ///     do_queries_part_1(&tx)?; // tx causes rollback if this fails
    ///     do_queries_part_2(&tx)?; // tx causes rollback if this fails
    ///
    ///     tx.commit()
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    #[inline]
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        Transaction::new(self, self.transaction_behavior)
    }

    /// Begin a new transaction with a specified behavior.
    ///
    /// See [`transaction`](Connection::transaction).
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    #[inline]
    pub fn transaction_with_behavior(
        &mut self,
        behavior: TransactionBehavior,
    ) -> Result<Transaction<'_>> {
        Transaction::new(self, behavior)
    }

    /// Run an operation inside a transaction with the requested behavior.
    ///
    /// Success commits the transaction. A callback or commit failure attempts
    /// rollback and returns both the original failure and any rollback error,
    /// together with the final connection state.
    pub fn with_transaction<T, E, F>(
        &mut self,
        behavior: TransactionBehavior,
        operation: F,
    ) -> Result<T, TransactionError<E>>
    where
        F: FnOnce(&Connection) -> Result<T, E>,
    {
        let mut transaction =
            Transaction::new_unchecked(self, behavior).map_err(|error| TransactionError {
                cause: TransactionFailure::Begin(error),
                rollback_error: None,
                rollback_attempted: false,
                release_error: None,
                is_autocommit: self.is_autocommit(),
            })?;

        let cause = match operation(&transaction) {
            Ok(value) => match transaction.commit_() {
                Ok(()) => return Ok(value),
                Err(error) => TransactionFailure::Commit(error),
            },
            Err(error) => TransactionFailure::Operation(error),
        };

        let rollback_attempted = !self.is_autocommit();
        let rollback_error = if rollback_attempted {
            transaction.rollback_().err().map(Box::new)
        } else {
            None
        };
        transaction.set_drop_behavior(DropBehavior::Ignore);

        Err(TransactionError {
            cause,
            rollback_error,
            rollback_attempted,
            release_error: None,
            is_autocommit: self.is_autocommit(),
        })
    }

    /// Begin a new transaction with the default behavior (DEFERRED).
    ///
    /// Attempt to open a nested transaction will result in a SQLite error.
    /// `Connection::transaction` prevents this at compile time by taking `&mut
    /// self`, but `Connection::unchecked_transaction()` may be used to defer
    /// the checking until runtime.
    ///
    /// See [`Connection::transaction`] and [`Transaction::new_unchecked`]
    /// (which can be used if the default transaction behavior is undesirable).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// # use std::rc::Rc;
    /// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
    /// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
    /// fn perform_queries(conn: Rc<Connection>) -> Result<()> {
    ///     let tx = conn.unchecked_transaction()?;
    ///
    ///     do_queries_part_1(&tx)?; // tx causes rollback if this fails
    ///     do_queries_part_2(&tx)?; // tx causes rollback if this fails
    ///
    ///     tx.commit()
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails. The specific
    /// error returned if transactions are nested is currently unspecified.
    pub fn unchecked_transaction(&self) -> Result<Transaction<'_>> {
        Transaction::new_unchecked(self, self.transaction_behavior)
    }

    /// Begin a new savepoint with the default behavior (DEFERRED).
    ///
    /// The savepoint defaults to rolling back when it is dropped. If you want
    /// the savepoint to commit, you must call [`commit`](Savepoint::commit) or
    /// [`set_drop_behavior(DropBehavior::Commit)`](Savepoint::set_drop_behavior).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
    /// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
    /// fn perform_queries(conn: &mut Connection) -> Result<()> {
    ///     let sp = conn.savepoint()?;
    ///
    ///     do_queries_part_1(&sp)?; // sp causes rollback if this fails
    ///     do_queries_part_2(&sp)?; // sp causes rollback if this fails
    ///
    ///     sp.commit()
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    #[inline]
    pub fn savepoint(&mut self) -> Result<Savepoint<'_>> {
        Savepoint::new(self)
    }

    /// Begin a savepoint using a shared reference to this connection.
    ///
    /// Unlike [`Connection::savepoint`], this cannot prevent overlapping
    /// savepoints at compile time. SQLite allows nested savepoints, but callers
    /// must finish them in reverse order.
    #[inline]
    pub fn unchecked_savepoint(&self) -> Result<Savepoint<'_>> {
        Savepoint::new_unchecked(self)
    }

    /// Run an operation inside a savepoint, releasing it on success.
    ///
    /// A callback or release failure rolls back the savepoint. The returned
    /// error preserves the original failure, cleanup errors, and final
    /// connection state. A failed rollback skips release so it cannot commit
    /// the failed operation.
    pub fn with_savepoint<T, E, F>(&self, operation: F) -> Result<T, TransactionError<E>>
    where
        F: FnOnce(&Connection) -> Result<T, E>,
    {
        let mut savepoint = self
            .unchecked_savepoint()
            .map_err(|error| TransactionError {
                cause: TransactionFailure::Begin(error),
                rollback_error: None,
                rollback_attempted: false,
                release_error: None,
                is_autocommit: self.is_autocommit(),
            })?;

        let cause = match operation(&savepoint) {
            Ok(value) => match savepoint.commit_() {
                Ok(()) => return Ok(value),
                Err(error) => TransactionFailure::Commit(error),
            },
            Err(error) => TransactionFailure::Operation(error),
        };

        let mut rollback_error = None;
        let mut release_error = None;
        let rollback_attempted = !self.is_autocommit();
        if rollback_attempted {
            match savepoint.rollback() {
                Ok(()) => release_error = savepoint.commit_().err().map(Box::new),
                Err(error) => rollback_error = Some(Box::new(error)),
            }
        }
        savepoint.set_drop_behavior(DropBehavior::Ignore);

        Err(TransactionError {
            cause,
            rollback_error,
            rollback_attempted,
            release_error,
            is_autocommit: self.is_autocommit(),
        })
    }

    /// Run an operation inside a savepoint and roll it back even on success.
    ///
    /// Returns the callback's value only after rollback and release succeed.
    /// Callback and cleanup errors are retained together. A failed rollback
    /// skips release so cleanup cannot commit the temporary changes.
    pub fn with_rollback_savepoint<T, E, F>(&self, operation: F) -> Result<T, TransactionError<E>>
    where
        F: FnOnce(&Connection) -> Result<T, E>,
    {
        let mut savepoint = self
            .unchecked_savepoint()
            .map_err(|error| TransactionError {
                cause: TransactionFailure::Begin(error),
                rollback_error: None,
                rollback_attempted: false,
                release_error: None,
                is_autocommit: self.is_autocommit(),
            })?;

        let result = operation(&savepoint);
        let rollback_error = savepoint.rollback().err().map(Box::new);
        let release_error = if rollback_error.is_none() {
            savepoint.commit_().err().map(Box::new)
        } else {
            None
        };
        savepoint.set_drop_behavior(DropBehavior::Ignore);

        let cause = match result {
            Ok(value) if rollback_error.is_none() && release_error.is_none() => return Ok(value),
            Ok(_) => TransactionFailure::Cleanup,
            Err(error) => TransactionFailure::Operation(error),
        };

        Err(TransactionError {
            cause,
            rollback_error,
            rollback_attempted: true,
            release_error,
            is_autocommit: self.is_autocommit(),
        })
    }

    /// Begin a new savepoint with a specified name.
    ///
    /// See [`savepoint`](Connection::savepoint).
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    #[inline]
    pub fn savepoint_with_name<T: Into<String>>(&mut self, name: T) -> Result<Savepoint<'_>> {
        Savepoint::with_name(self, name)
    }

    /// Begin a named savepoint using a shared reference to this connection.
    ///
    /// Like [`Connection::unchecked_savepoint`], callers must finish
    /// overlapping savepoints in reverse order.
    #[inline]
    pub fn unchecked_savepoint_with_name<T: Into<String>>(&self, name: T) -> Result<Savepoint<'_>> {
        Savepoint::with_name_unchecked(self, name)
    }

    /// Determine the transaction state of a database
    pub fn transaction_state<N: crate::Name>(
        &self,
        db_name: Option<N>,
    ) -> Result<TransactionState> {
        self.db.borrow().txn_state(db_name)
    }

    /// Set the default transaction behavior for the connection.
    ///
    /// ## Note
    ///
    /// This will only apply to transactions initiated by [`transaction`](Connection::transaction)
    /// or [`unchecked_transaction`](Connection::unchecked_transaction).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result, TransactionBehavior};
    /// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
    /// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
    /// fn perform_queries(conn: &mut Connection) -> Result<()> {
    ///     conn.set_transaction_behavior(TransactionBehavior::Immediate);
    ///
    ///     let tx = conn.transaction()?;
    ///
    ///     do_queries_part_1(&tx)?; // tx causes rollback if this fails
    ///     do_queries_part_2(&tx)?; // tx causes rollback if this fails
    ///
    ///     tx.commit()
    /// }
    /// ```
    pub fn set_transaction_behavior(&mut self, behavior: TransactionBehavior) {
        self.transaction_behavior = behavior;
    }
}

#[cfg(all(test, not(miri)))]
mod test {
    use std::assert_matches;
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{DropBehavior, TransactionBehavior, TransactionFailure};
    use crate::{Connection, Error, Result};

    fn checked_memory_handle() -> Result<Connection> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        Ok(db)
    }

    #[test]
    fn test_drop() -> Result<()> {
        let mut db = checked_memory_handle()?;
        {
            let tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            // default: rollback
        }
        {
            let mut tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(2)")?;
            tx.set_drop_behavior(DropBehavior::Commit);
        }
        {
            let tx = db.transaction()?;
            assert_eq!(2, tx.one_column::<i32, _>("SELECT SUM(x) FROM foo", [])?);
        }
        Ok(())
    }

    #[test]
    fn failed_finish() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE parent(id INTEGER PRIMARY KEY);
             CREATE TABLE child(parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);",
        )?;

        let mut transaction = db.transaction()?;
        transaction.set_drop_behavior(DropBehavior::Commit);
        transaction.execute("INSERT INTO child VALUES (42)", [])?;
        transaction.finish().unwrap_err();

        let mut savepoint = db.unchecked_savepoint()?;
        savepoint.set_drop_behavior(DropBehavior::Commit);
        savepoint.execute("INSERT INTO child VALUES (42)", [])?;
        savepoint.finish().unwrap_err();
        Ok(())
    }

    #[test]
    fn scoped_savepoint() -> Result<()> {
        let db = checked_memory_handle()?;
        let error = db
            .with_savepoint(|conn| {
                conn.execute("INSERT INTO foo VALUES (1)", [])
                    .map_err(|_| 0_u8)?;

                Err::<(), _>(42_u8)
            })
            .unwrap_err();

        assert_matches!(error.cause, TransactionFailure::Operation(42));
        assert_eq!(db.one_column::<i32, _>("SELECT count(*) FROM foo", [])?, 0);
        Ok(())
    }

    #[test]
    fn scoped_commit_error() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE parent(id INTEGER PRIMARY KEY);
             CREATE TABLE child(parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);",
        )?;
        let error = db
            .with_transaction(TransactionBehavior::Immediate, |conn| {
                conn.execute("INSERT INTO child VALUES (42)", [])
            })
            .unwrap_err();

        assert_matches!(error.cause, TransactionFailure::Commit(_));
        Ok(())
    }

    #[test]
    fn savepoint_cleanup_error() -> Result<()> {
        let db = checked_memory_handle()?;
        db.execute_batch("BEGIN")?;
        let error = db
            .with_savepoint(|conn| {
                conn.execute_batch("RELEASE _rusqlite_sp")
                    .map_err(|_| 0_u8)?;

                Err::<(), _>(42_u8)
            })
            .unwrap_err();

        assert!(error.rollback_error.is_some());
        db.execute_batch("ROLLBACK")
    }
    fn assert_nested_tx_error(e: Error) {
        assert_matches!(
            e,
            Error::SqliteFailure(
                crate::ffi::Error {
                    code: crate::ErrorCode::Unknown,
                    extended_code: crate::ffi::SQLITE_ERROR,
                },
                Some(msg),
            ) if msg.contains("transaction")
        );
    }

    #[test]
    fn test_unchecked_nesting() -> Result<()> {
        let db = checked_memory_handle()?;

        {
            let tx = db.unchecked_transaction()?;
            let e = tx.unchecked_transaction().unwrap_err();
            assert_nested_tx_error(e);
            // default: rollback
        }
        {
            let tx = db.unchecked_transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            // Ensure this doesn't interfere with ongoing transaction
            let e = tx.unchecked_transaction().unwrap_err();
            assert_nested_tx_error(e);

            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            tx.commit()?;
        }

        assert_eq!(2, db.one_column::<i32, _>("SELECT SUM(x) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_explicit_rollback_commit() -> Result<()> {
        let mut db = checked_memory_handle()?;
        {
            let mut tx = db.transaction()?;
            {
                let mut sp = tx.savepoint()?;
                sp.execute_batch("INSERT INTO foo VALUES(1)")?;
                sp.rollback()?;
                sp.execute_batch("INSERT INTO foo VALUES(2)")?;
                sp.commit()?;
            }
            tx.commit()?;
        }
        {
            let tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(4)")?;
            tx.commit()?;
        }
        {
            let tx = db.transaction()?;
            assert_eq!(6, tx.one_column::<i32, _>("SELECT SUM(x) FROM foo", [])?);
        }
        Ok(())
    }

    #[test]
    fn test_savepoint() -> Result<()> {
        let mut db = checked_memory_handle()?;
        {
            let mut tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            assert_current_sum(1, &tx)?;
            tx.set_drop_behavior(DropBehavior::Commit);
            {
                let mut sp1 = tx.savepoint()?;
                sp1.execute_batch("INSERT INTO foo VALUES(2)")?;
                assert_current_sum(3, &sp1)?;
                // will roll back sp1
                {
                    let mut sp2 = sp1.savepoint()?;
                    sp2.execute_batch("INSERT INTO foo VALUES(4)")?;
                    assert_current_sum(7, &sp2)?;
                    // will roll back sp2
                    {
                        let sp3 = sp2.savepoint()?;
                        sp3.execute_batch("INSERT INTO foo VALUES(8)")?;
                        assert_current_sum(15, &sp3)?;
                        sp3.commit()?;
                        // committed sp3, but will be erased by sp2 rollback
                    }
                    assert_current_sum(15, &sp2)?;
                }
                assert_current_sum(3, &sp1)?;
            }
            assert_current_sum(1, &tx)?;
        }
        assert_current_sum(1, &db)?;
        Ok(())
    }

    #[test]
    fn test_ignore_drop_behavior() -> Result<()> {
        let mut db = checked_memory_handle()?;

        let mut tx = db.transaction()?;
        {
            let mut sp1 = tx.savepoint()?;
            insert(1, &sp1)?;
            sp1.rollback()?;
            insert(2, &sp1)?;
            {
                let mut sp2 = sp1.savepoint()?;
                sp2.set_drop_behavior(DropBehavior::Ignore);
                insert(4, &sp2)?;
            }
            assert_current_sum(6, &sp1)?;
            sp1.commit()?;
        }
        assert_current_sum(6, &tx)?;
        Ok(())
    }

    #[test]
    fn test_savepoint_drop_behavior_releases() -> Result<()> {
        let mut db = checked_memory_handle()?;

        {
            let mut sp = db.savepoint()?;
            sp.set_drop_behavior(DropBehavior::Commit);
        }
        assert!(db.is_autocommit());
        {
            let mut sp = db.savepoint()?;
            sp.set_drop_behavior(DropBehavior::Rollback);
        }
        assert!(db.is_autocommit());

        Ok(())
    }

    #[test]
    fn test_savepoint_release_error() -> Result<()> {
        let mut db = checked_memory_handle()?;

        db.pragma_update(None, "foreign_keys", true)?;
        db.execute_batch("CREATE TABLE r(n INTEGER PRIMARY KEY NOT NULL); CREATE TABLE f(n REFERENCES r(n) DEFERRABLE INITIALLY DEFERRED);")?;
        {
            let mut sp = db.savepoint()?;
            sp.execute("INSERT INTO f VALUES (0)", [])?;
            sp.set_drop_behavior(DropBehavior::Commit);
        }
        assert!(db.is_autocommit());

        Ok(())
    }

    #[test]
    fn test_savepoint_names() -> Result<()> {
        let mut db = checked_memory_handle()?;

        {
            let mut sp1 = db.savepoint_with_name("my_sp")?;
            insert(1, &sp1)?;
            assert_current_sum(1, &sp1)?;
            {
                let mut sp2 = sp1.savepoint_with_name("my_sp")?;
                sp2.set_drop_behavior(DropBehavior::Commit);
                insert(2, &sp2)?;
                assert_current_sum(3, &sp2)?;
                sp2.rollback()?;
                assert_current_sum(1, &sp2)?;
                insert(4, &sp2)?;
            }
            assert_current_sum(5, &sp1)?;
            sp1.rollback()?;
            {
                let mut sp2 = sp1.savepoint_with_name("my_sp")?;
                sp2.set_drop_behavior(DropBehavior::Ignore);
                insert(8, &sp2)?;
            }
            assert_current_sum(8, &sp1)?;
            sp1.commit()?;
        }
        assert_current_sum(8, &db)?;
        Ok(())
    }

    #[test]
    fn test_unchecked_savepoint() -> Result<()> {
        let db = checked_memory_handle()?;
        {
            let sp = db.unchecked_savepoint()?;
            insert(1, &sp)?;
            {
                let nested = db.unchecked_savepoint_with_name("nested")?;
                insert(2, &nested)?;
            }
            assert_current_sum(1, &sp)?;
            sp.commit()?;
        }
        assert_current_sum(1, &db)?;

        Ok(())
    }

    #[test]
    fn test_rc() -> Result<()> {
        use std::rc::Rc;
        let mut conn = Connection::open_in_memory()?;
        let rc_txn = Rc::new(conn.transaction()?);

        // This will compile only if Transaction is Debug
        Rc::try_unwrap(rc_txn).unwrap();
        Ok(())
    }

    fn insert(x: i32, conn: &Connection) -> Result<usize> {
        conn.execute("INSERT INTO foo VALUES(?1)", [x])
    }

    fn assert_current_sum(x: i32, conn: &Connection) -> Result<()> {
        assert_eq!(x, conn.one_column::<i32, _>("SELECT SUM(x) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn txn_state() -> Result<()> {
        use super::TransactionState;
        use crate::{DEFAULT_NAME, MAIN_DB};
        let db = Connection::open_in_memory()?;
        assert_eq!(TransactionState::None, db.transaction_state(Some(MAIN_DB))?);
        assert_eq!(TransactionState::None, db.transaction_state(DEFAULT_NAME)?);
        db.execute_batch("BEGIN")?;
        assert_eq!(TransactionState::None, db.transaction_state(DEFAULT_NAME)?);
        let _: i32 = db.pragma_query_one(None, "user_version")?;
        assert_eq!(TransactionState::Read, db.transaction_state(DEFAULT_NAME)?);
        db.pragma_update(None, "user_version", 1)?;
        assert_eq!(TransactionState::Write, db.transaction_state(DEFAULT_NAME)?);
        db.execute_batch("ROLLBACK")?;
        Ok(())
    }

    #[test]
    fn auto_commit() -> Result<()> {
        use super::TransactionState;
        use crate::DEFAULT_NAME;
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE t(i UNIQUE);")?;
        assert!(db.is_autocommit());
        let mut stmt = db.prepare("SELECT name FROM sqlite_master")?;
        assert_eq!(TransactionState::None, db.transaction_state(DEFAULT_NAME)?);
        {
            let mut rows = stmt.query([])?;
            assert!(rows.next()?.is_some()); // start reading
            assert_eq!(TransactionState::Read, db.transaction_state(DEFAULT_NAME)?);
            db.execute("INSERT INTO t VALUES (1)", [])?; // auto-commit
            assert_eq!(TransactionState::Read, db.transaction_state(DEFAULT_NAME)?);
            assert!(rows.next()?.is_some()); // still reading
            assert_eq!(TransactionState::Read, db.transaction_state(DEFAULT_NAME)?);
            assert!(rows.next()?.is_none()); // end
            assert_eq!(TransactionState::None, db.transaction_state(DEFAULT_NAME)?);
        }
        Ok(())
    }
}
