# Fork patches

This fork adds query, transaction, and session helpers, plus fixes for conversion errors, callbacks, streaming input, cached statements, library selection, and timestamp parsing. Related changes stay grouped in a few commits on the `fork` branch.

The upstream base is rusqlite/rusqlite commit 84beed6601ec2e261fea83c07834f5d861a9884a. Codec bindings and rarray rowid ordering are already part of that base. The patches here carry forward the earlier `extras` branch and include the streaming reader fix from the Snak vendored copy.

## Using the fork

```toml
[dependencies]
rusqlite = { git = "https://github.com/Viterkim/rusqlite", branch = "fork", features = ["bundled", "session", "buildtime_bindgen"] }
```

Use a `rev` instead of `branch` to pin a particular commit. The package version remains 0.41.0, so that number alone does not identify these patches. The normal upstream feature flags still apply. Session additions use `session`, and native session builds need libclang for bindgen.

The API notes below cover differences from upstream. For the added APIs' generated docs, run:

```sh
cargo doc --features "bundled session buildtime_bindgen" --open
```

## Shared connection APIs (14ab86f)

### Savepoints

`Connection::unchecked_savepoint` and `unchecked_savepoint_with_name` let code holding a shared connection open a savepoint. `Savepoint::new_unchecked` and `with_name_unchecked` provide the same access through constructors.

The usual commit and rollback behavior stays the same. Callers must finish overlapping savepoints in reverse order because the shared reference cannot enforce that ordering at compile time.

### Hooks

`progress_handler` and `authorizer` take `&Connection`. This lets code install those hooks while a session or other shared borrow is alive. Requires the existing `hooks` feature.

Changing or removing either hook while one of these callbacks is running returns an error, keeping the running callback alive. Recursive callback entry is rejected before borrowing the closure again.

## Session extensions and safety fixes (4dde5ba)

### Diff inference and errors

`Session::diff` drops an unused generic parameter that Rust could not infer. Calls such as `session.diff("baseline", "items")` now work directly. Code supplying two explicit generic arguments needs to supply only the name type.

Diff errors also work when SQLite supplies no message, such as an authorizer denying an internal query. These return an error without dereferencing a null pointer.

### Changeset size tracking

`Session::set_changeset_size_tracking` enables SQLite's size tracking, and `changeset_size` returns an upper bound on the changeset's byte length. Enable it before attaching tables. Tracking adds overhead while recording changes, and the size is zero when tracking is disabled.

### Session lifetimes and borrowed callbacks

The streaming iterator owns a stable callback context for as long as SQLite can read from it. Moving the iterator keeps that context valid, and the reader stays borrowed until the iterator is dropped.

`ChangesetItem` gains a lifetime so a conflict item cannot escape its callback. Apply callbacks accept borrowed `FnMut` closures without `Send` or `'static` bounds, so they can update local state directly. `ChangesetIter::next` is also available without importing the streaming iterator trait.

The streaming constructor changes from `start_strm(&input)` with an extra reference to `start_strm(&mut input)`. Explicit item types become `ChangesetItem<'_>`. Returning or storing an owned conflict item outside its callback is rejected.

An iteration error clears the current item, so `get` cannot expose a row SQLite has already invalidated.

### Changeset inspection

`new_value_opt`, `old_value_opt`, and `conflict_value_opt` return `Option<ValueRef>` for values that may be absent. An unchanged column can be absent from an update, which is different from SQL NULL. Invalid operations and column indices still return errors.

`Operation::changeset_operation` returns a typed `ChangesetOperation` with `Insert`, `Update`, and `Delete` variants. `column_count` returns a checked `usize`. The existing accessors remain available.

All column value accessors check that an index fits SQLite's integer range, so a huge index cannot wrap around and read another column.

### Streaming I/O safety

SQLite may supply an uninitialized input buffer. The wrapper initializes it before giving a safe Rust `Read` implementation access to the bytes, and rejects a reader that reports a count larger than the supplied buffer.

Reader and writer panics are caught inside the callbacks and returned to SQLite as I/O errors. These fixes apply to every session API using the shared streaming callbacks, including iteration, apply, changegroups, and rebasing.

### Filter panic handling

A panicking apply filter now makes the apply return an error. Previously the panic was swallowed and treated as a request to skip the table, so a partial apply could report success.

Filtered applies use an outer savepoint to undo earlier changes when the filter panics. `NOSAVEPOINT` disables that protection too, so callers using that flag must handle rollback themselves.

### V2 apply flags

`Connection::apply_with_flags` and `apply_strm_with_flags` expose SQLite's v2 apply APIs for buffered and streamed changesets. `ChangesetApplyFlags` provides `NOSAVEPOINT`, `INVERT`, `IGNORENOOP`, and `FKNOACTION`.

These methods discard rebase output. SQLite marks the v2 API as experimental.

### Changeset rebasing

`apply_with_flags_and_rebase` and `apply_strm_with_flags_and_rebase` return optional rebase data from conflict resolution. Feed that data to `Rebaser::configure`, then use `rebase` or `rebase_strm` to adjust local changesets before sending them to peers.

Configure the rebaser in the order the remote changesets were applied. SQLite marks its rebaser API as experimental.

### Borrowed changeset bytes

`ChangesetIter::start_bytes` iterates directly over a borrowed byte slice. `Connection::apply_bytes_with_flags` applies one directly with v2 flags, avoiding a streaming reader or an intermediate owned changeset. Iteration keeps the bytes borrowed for the iterator's lifetime, and apply borrows them for the duration of the call.

## Other fixes

### Cached statements

Bundled native SQLite includes the upstream [535a77cf5a fix](https://github.com/sqlite/sqlite/commit/78793bee814300ff98a48970edfed9189a64f5c6) for repeated statement compilation after clearing bindings. This fixes [rusqlite #1931](https://github.com/rusqlite/rusqlite/issues/1931) while keeping SQLite's optimizations enabled. It applies to this fork's bundled SQLite source, with the reported version still 3.54.1.

### Explicit library directories

`SQLITE3_LIB_DIR` and `SQLCIPHER_LIB_DIR` take precedence over system pkg-config results, fixing [#510](https://github.com/rusqlite/rusqlite/issues/510). Dependency flags are read from a `.pc` file beside the selected library when one exists. Static linking requests also apply to that metadata lookup.

### Invalid timestamps

Reading an `OffsetDateTime` with the `time` feature returns a conversion error for malformed text, including short strings and Unicode at the date boundary. It no longer panics while looking for a timezone offset.

## Query, conversion, and callback errors

### Query helpers

`Connection::query_all` collects decoded rows. `query_first` reads and decodes only the first row, returning `None` for an empty result. Both accept a normal fallible decoder with any error type, with no required conversion into a rusqlite error.

`QueryError<E>` keeps SQLite failures and decoder failures distinct, retains the SQL, and identifies a failed decoder's result row. The original decoder error stays in its `Decode` variant. Both helpers use the statement cache when the `cache` feature is enabled.

The new query, transaction, and fallible apply helpers accept any callback error value. Their error wrappers implement `std::error::Error` when that value does. Containers such as `anyhow::Error`, boxed trait objects, and er trees need explicit variant handling to recover their original errors. These APIs do not add a dependency on any error crate or automatically bridge its reporting system.

For existing rusqlite decoders, `execute_cached`, `query_row_cached`, and `query_one_cached` provide cached versions of the usual connection helpers. Ordinary `execute` and `query_row` keep their existing preparation behavior.

### Column context

`Row::get` conversion errors now use `Error::ColumnConversionFailure`. Its `ColumnError` records the column index, name, stored SQLite type, requested Rust type, and original conversion error. The original parser error remains reachable through the standard source chain.

Code matching the old conversion variants from `Row::get` should inspect `ColumnError::source` instead. Column name and index lookup errors are unchanged. Conversions outside a row use `Error::ValueConversionFailure`, so missing context is no longer represented by a fake column index or SQL NULL type.

`types::from_sql_try` reads an existing SQL representation and applies `TryFrom`, simplifying validated newtypes. `FromSqlError::other` also accepts an existing boxed error without boxing it again.

### Session callbacks and owned rows

Streaming operations retain their original I/O errors in `Error::SessionIoFailure`, including failures encountered after starting a streaming iterator. The SQLite error code is retained too.

`Connection::apply_bytes_try` accepts a conflict callback returning `Result<ConflictAction, E>`. A callback error aborts the apply and returns the original value alongside SQLite's outcome in `SessionApplyError<E>`. The callback error can be any type.

`ChangesetItem::to_owned` copies row metadata, primary key flags, and old/new values into an `OwnedChangesetRow`. Absent update fields stay distinct from SQL NULL. A foreign key conflict has no row to copy and returns an error.

### Scoped transactions and savepoints

`Connection::with_transaction` runs a fallible operation with the requested transaction behavior, including `Immediate`. `with_savepoint` supports nested operations through a shared connection. Both accept any callback error type. `TransactionError<E>` preserves the original failure, rollback and release failures, and the final autocommit state. A failed rollback skips savepoint release so cleanup cannot accidentally commit the failed operation.

Explicit transaction and savepoint `finish` calls retain a failed commit error even when rollback succeeds. They no longer make a second hidden cleanup attempt through Drop after reporting their result.

### Encryption methods

With the `codec` feature, `Connection::set_key` and `rekey` provide distinct methods for installing and changing an encryption key. The earlier `key` method with its boolean argument remains available.
