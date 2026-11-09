# Fork patches

Based on rusqlite/rusqlite commit 9f59583916e4667f12671150dc42b2b1910433f7, with the earlier `extras` changes and the streaming reader fix from Snak. Codec bindings, rarray rowid ordering, null safe session diff errors, protected raw values, and callback cleanup after client data allocation failure are already upstream.

## Using the fork

```toml
[dependencies]
rusqlite = { git = "https://github.com/Viterkim/rusqlite", branch = "fork", features = ["bundled", "session", "buildtime_bindgen"] }
```

Use `rev` to pin a commit. The package version is still 0.41.0, so the version alone does not identify these patches. Upstream feature flags still apply. Session additions need `session`, and native session builds need libclang for bindgen.

The notes below cover changes from upstream. To read the generated API docs:

```sh
cargo doc --features "bundled session buildtime_bindgen" --open
```

## Shared connection APIs

### Savepoints

`Connection::unchecked_savepoint` and `unchecked_savepoint_with_name` open savepoints through `&Connection`. The constructors `Savepoint::new_unchecked` and `with_name_unchecked` do the same.

Finish overlapping savepoints in reverse order. Commit and rollback work as usual, but the shared reference cannot enforce the ordering.

### Hooks

With `hooks`, `progress_handler` and `authorizer` take `&Connection`, so you can install them while a session or another shared borrow is alive.

Changing or removing a running hook returns an error so its callback stays alive. Recursive entry is rejected before borrowing the closure again.

## Session extensions and safety fixes

### Diff and failed capture

`Session::diff` uses upstream's inferred name types and null safe error handling. The fork checks capture before diffing, so a failed capture cannot resume through this call.

### Changeset size tracking

Enable `Session::set_changeset_size_tracking` before attaching tables. `changeset_size` then returns an upper bound on the changeset's byte length. Tracking adds overhead while recording changes and reports zero when disabled.

### Session lifetimes and borrowed callbacks

Streaming iterators keep their callback context at a stable address when moved and borrow the reader until dropped.

`ChangesetItem` gains a lifetime so a conflict item cannot escape its callback. Apply callbacks accept borrowed `FnMut` closures without `Send` or `'static` bounds, so they can update local state directly. `ChangesetIter::next` is also available without importing the streaming iterator trait.

The streaming constructor takes `start_strm(&mut input)` instead of an extra reference through `start_strm(&input)`. Explicit item types become `ChangesetItem<'_>`.

An iteration error clears the current item, so `get` cannot expose a row SQLite has already invalidated.

### Changeset inspection

`new_value_opt`, `old_value_opt`, and `conflict_value_opt` return `Option<ValueRef>` for values that may be absent. An unchanged column can be absent from an update, which is different from SQL NULL. Invalid operations and column indices still return errors.

`Operation::changeset_operation` returns `ChangesetOperation::{Insert, Update, Delete}`, and `column_count` returns a checked `usize`. The existing accessors remain available.

All column value accessors check that an index fits SQLite's integer range, so a huge index cannot wrap around and read another column.

### Streaming I/O safety

SQLite may supply an uninitialized input buffer. The wrapper initializes it before giving a safe Rust `Read` implementation access to the bytes, and rejects a reader that reports a count larger than the supplied buffer.

Reader and writer panics are caught inside the callbacks and returned to SQLite as I/O errors. These fixes apply to every session API using the shared streaming callbacks, including iteration, apply, changegroups, and rebasing.

### Filter panic handling

An apply filter panic returns an error. Swallowing it as a request to skip the table could let a partial apply report success.

Filtered applies use an outer savepoint to undo earlier changes after a filter panic. With `NOSAVEPOINT`, you must handle rollback yourself.

A `Session::table_filter` panic permanently invalidates capture. Replacing or removing the filter does not restore it, and changeset and patchset extraction return `Error::SessionCaptureFailure`. Check `Session::check_capture` before using `is_empty` to skip extraction. Failed capture needs a new session.

Reentrant capture filter replacement is rejected by invalidating capture. Dropping a session from its running capture filter intentionally leaks its native session and callback allocation, keeping SQLite's active memory alive.

### V2 apply flags

`Connection::apply_with_flags` and `apply_strm_with_flags` expose SQLite's v2 apply APIs for buffered and streamed changesets. `ChangesetApplyFlags` provides `NOSAVEPOINT`, `INVERT`, `IGNORENOOP`, and `FKNOACTION`.

These methods discard rebase output. SQLite marks the v2 API as experimental.

### Changeset rebasing

`apply_with_flags_and_rebase` and `apply_strm_with_flags_and_rebase` return optional rebase data from conflict resolution. Feed it to `Rebaser::configure` in the order the remote changesets were applied, then use `rebase` or `rebase_strm` to adjust local changesets before sending them to peers. SQLite marks this API as experimental.

### Borrowed changeset bytes

`ChangesetIter::start_bytes` iterates over a borrowed byte slice, and `Connection::apply_bytes_with_flags` applies one with v2 flags. Neither needs a streaming reader or an intermediate owned changeset. The bytes stay borrowed until the iterator is dropped or apply returns.

## Other fixes

### Cached statements

The bundled native SQLite source includes the upstream [535a77cf5a fix](https://github.com/sqlite/sqlite/commit/78793bee814300ff98a48970edfed9189a64f5c6) for repeated statement compilation after clearing bindings ([rusqlite #1931](https://github.com/rusqlite/rusqlite/issues/1931)). SQLite's optimizations stay enabled and its reported version is still 3.54.1.

### Explicit library directories

`SQLITE3_LIB_DIR` and `SQLCIPHER_LIB_DIR` take precedence over system pkg-config results, fixing [#510](https://github.com/rusqlite/rusqlite/issues/510). Dependency flags are read from a `.pc` file beside the selected library when one exists. Static linking requests also apply to that metadata lookup.

### Invalid timestamps

Reading an `OffsetDateTime` with the `time` feature returns a conversion error for malformed text, including short strings and Unicode at the date boundary. It no longer panics while looking for a timezone offset.

### Pointer values

Pointer outputs keep owned data alive through cloning and binding, then release it when no longer in use. `ToSqlOutput::from_arc` replaces `from_rc`; `new_boxed` also requires `Send + Sync + 'static` data. Both constructors are unsafe because the SQLite pointer type name must match how its consumers use the data. `from_raw` passes a borrowed pointer without a destructor and requires the data to stay alive through every use.

`vtab::array::Array` is now `Arc<Vec<Value>>` instead of `Rc<Vec<Value>>`, so a connection can release its bindings on another thread.

### Column metadata and busy timeouts

`Connection::column_metadata` and `Statement::column_metadata` return owned `CString` values. SQLite only keeps the type and collation pointers until the next SQLite API call.

`Statement::column_name` returns `String`, and `column_names` yields `String`s. `Column` and `ColumnMetadata` own their strings and no longer take a lifetime parameter. This follows SQLite's documented name lifetime ([#913](https://github.com/rusqlite/rusqlite/issues/913)) and adds copies when requesting names or metadata. Named row lookups still compare names without copying them.

`busy_timeout` returns `Error::BusyTimeoutOutOfRange` when the whole millisecond count exceeds SQLite's limit.

`Filters::in_values` returns owned `Value`s. SQLite invalidates each element when the IN iterator advances. Invalid UTF-8 in a text element returns a conversion error.

## Query, conversion, and callback errors

### Query helpers

`Connection::query_all` collects decoded rows. `query_first` decodes the first row and returns `None` for an empty result. Both accept fallible decoders with any error type, so your decoder errors need no conversion into rusqlite errors.

`QueryError<E>` retains the SQL and keeps the original decoder error in `Decode`, with its result row index. SQLite failures use `Sql`. Both helpers use the statement cache with `cache` enabled.

`Decode::finish_error` retains any statement completion failure alongside the decoder error. The usual `query_row` and `query_one` helpers preserve simultaneous failures in `Error::StatementCompletionFailure`. The legacy `query_row_and_then` returns decoder errors directly as `E`, so use `query_first` when both failures must survive.

Query, transaction, and fallible apply helpers accept any callback error value. Their wrappers implement `std::error::Error` when that value does. Recover errors held in `anyhow::Error`, boxed trait objects, or er trees by matching the wrapper variants. The helpers have no error crate dependency or automatic bridge to those reporting systems.

`execute_cached`, `query_row_cached`, and `query_one_cached` provide cached versions of the usual connection helpers for existing rusqlite decoders. Ordinary `execute` and `query_row` keep their preparation behavior.

First row helpers check statement completion before reporting success. A write with `RETURNING` can produce a row and then fail to commit, and that failure is now returned. Manual row iteration can use `Rows::finish` to check completion without reading the remaining rows.

`execute_batch` also checks statement finalization before reporting success, including batches with `RETURNING`.

`execute` rejects prepared statements with result columns before stepping them, including writes with `RETURNING` and queries that return no rows. Use the query methods for those statements.

### Column context

`Row::get` conversion errors use `Error::ColumnConversionFailure`. Its `ColumnError` records the column index, name, stored SQLite type, requested Rust type, and original conversion error. Parser errors remain reachable through the standard source chain.

Code matching the old conversion variants from `Row::get` should inspect `ColumnError::source`. Column name and index lookup errors are unchanged. Conversions outside a row use `Error::ValueConversionFailure`, without a fake column index or SQL NULL type.

`types::from_sql_try` reads an existing SQL representation and applies `TryFrom`, simplifying validated newtypes. `FromSqlError::other` also accepts an existing boxed error without boxing it again.

`Row::get_blob` and `get_text` borrow bytes or UTF-8 text without copying, while retaining the same column conversion context. Numeric parameter indices are checked before conversion to SQLite's integer type. SQLite error code accessors also cover `SqlInputError`.

`Row::get_blob_opt` borrows a nullable BLOB, returning `None` for SQL NULL and preserving column context for invalid types. Callers can check byte limits before copying the value.

Oversized column, database, and virtual table constraint indices cannot wrap around to another index. Blob seeks reject offsets that would overflow the old position calculation.

### Session callbacks and owned rows

Streaming operations retain their original I/O errors in `Error::SessionIoFailure`, including failures encountered after starting a streaming iterator. The SQLite error code is retained too.

`Connection::apply_bytes_try` accepts a conflict callback returning `Result<ConflictAction, E>`. A callback error aborts apply and keeps the original error alongside SQLite's outcome in `SessionApplyError<E>`.

`Error::SessionApplyFailure` retains the original apply error, streaming I/O error, filter panic, rollback attempt, cleanup failures, and final autocommit state when filter or cleanup failures occur. A failed rollback skips release. Cleanup failures no longer overwrite the original failure.

`ChangesetItem::to_owned` copies row metadata, primary key flags, and old/new values into an `OwnedChangesetRow`. Absent update fields stay distinct from SQL NULL. A foreign key conflict has no row to copy and returns an error.

`Session::changeset_bytes(max_bytes)` collects streamed output with a caller supplied byte limit and fallible buffer allocation. Its `ChangesetBytesError` distinguishes a size limit from allocation or SQLite failures. The limit applies to emitted bytes, so an upper bound from size tracking does not reject output that would fit. `Changeset::as_bytes` borrows an existing encoded changeset.

Commit hook panics veto the commit, and progress handler panics interrupt the operation. WAL hook panics now return an error too. WAL callbacks run after commit, so their failure does not undo committed data.

### Scoped transactions and savepoints

`Connection::with_transaction` runs a fallible operation with the requested transaction behavior, including `Immediate`. Use `with_savepoint` for nested operations through a shared connection. `TransactionError<E>` keeps the original error, rollback and release failures, and final autocommit state. A failed rollback skips savepoint release so cleanup cannot accidentally commit the failed operation.

Transaction callbacks now receive `&Transaction`, which dereferences to `Connection` and allows helpers to require a transaction in their signatures. Explicit callbacks expecting `&Connection` need a closure or an updated signature.

`with_transaction_decision` and `with_savepoint_decision` let the callback return `TransactionDecision::Commit(value)` or `Rollback(value)`. A rollback can return a normal domain result, such as rejected replay conflicts. Both decisions return the value only after cleanup succeeds. For savepoints, `Commit` releases the savepoint and `Rollback` rolls back then releases it. The savepoint callback receives `&Savepoint`. The existing scope helpers use the same completion paths.

`TransactionError::rollback_attempted` distinguishes a successful rollback from skipped cleanup. `with_rollback_savepoint` always rolls back, including on callback success, and returns the callback's value only after rollback and release succeed. Cleanup failure after a successful callback uses `TransactionFailure::Cleanup`.

Explicit transaction and savepoint `finish` calls keep a failed commit error even when rollback succeeds, without another hidden cleanup attempt through Drop.

### Encryption methods

With the `codec` feature, `Connection::set_key` and `rekey` provide distinct methods for installing and changing an encryption key. The earlier `key` method with its boolean argument remains available.

## File opening and connection settings

`Connection::open_file` and `open_file_with_flags` open literal filenames, including names beginning with `file:` or equal to `:memory:`. Relative paths are resolved first so SQLite's global URI setting cannot reinterpret them. Use `open` for URI names or `open_in_memory` for an in-memory database.

`set_foreign_keys`, `set_trusted_schema`, `set_journal_mode`, and `set_synchronous` verify that the requested setting is active. Journal and synchronization modes use `JournalMode` and `Synchronous`. An ignored request returns `Error::PragmaValueMismatch` with the requested and actual values. Journal and synchronization setters apply to the main database.

## Backup cancellation

`Backup::run_to_completion_with` accepts a borrowed `FnMut` callback returning any error type. It receives `StepResult` and progress, so it can cancel or stop retrying busy databases. `BackupError<E>` keeps the callback error unchanged. The callback runs after `Done` too, when the backup is already complete. Both completion helpers reject nonpositive page counts.

`run_to_completion_checked` runs a fallible check before every step, including the first, and reports progress afterward. A failed check stops it before copying more pages.

## Schema and connection metadata

`Connection::table_columns(schema, table)` returns `TableColumn` records, including generated and hidden columns, primary key ordering, and default expressions as SQL text. Names are bound as parameters. This covers [#991](https://github.com/rusqlite/rusqlite/issues/991).

`set_last_insert_rowid` exposes SQLite's connection metadata setter requested in [#1757](https://github.com/rusqlite/rusqlite/issues/1757). `system_errno` exposes the OS error number for an open connection's latest I/O failure, requested in [#458](https://github.com/rusqlite/rusqlite/issues/458).

## Deserialization safety

With `serialize`, reader buffers are initialized before a safe Rust reader receives them. Read failures retain their original `io::Error` in `Error::DeserializationReadFailure`. Invalid schemas and sizes are checked before transferring buffer ownership, fixing an allocation leak.

Deserialization buffers include initialized padding for malformed input, as [SQLite recommends](https://sqlite.org/c3ref/deserialize.html). `deserialize_bytes` now copies its input into that owned buffer and accepts an ordinary borrowed slice instead of requiring a static slice.
