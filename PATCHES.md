# Fork patches

This fork adds savepoint and session APIs, plus fixes for Rust callbacks and streaming input. The `fork` branch groups related changes into shared connection APIs, session changes, and fork documentation.

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
