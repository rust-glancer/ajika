# Acquiring inputs on demand

An application can know that thousands of files exist without needing all their
text. An input initializer lets it capture a field when the first query reads it.
Later reads of that captured field receive the same observation.

## Capture once, then read

The initializer receives the input key and an `InputInit` context. For a file
keyed by an interned ID, use the context's interner to look up its path.

```rust
# extern crate ajika;
use ajika::{InputInit, Result};

#[ajika::interned]
struct FileId {
    path: String,
}

fn acquire_text(id: &FileId, cx: &dyn InputInit) -> Result<String> {
    cx.check_cancelled()?;
    let path = id.path(&cx.interner())?;
    std::fs::read_to_string(path).map_err(ajika::Error::application)
}

#[ajika::input(key = FileId)]
struct File {
    #[input(arc, initialize = acquire_text)]
    text: String,
}
```

Treat lazy acquisition as an optimization. A read or cache validation can run
`acquire_text` whenever it needs an uncaptured key. A successful capture becomes
visible to the application, including through views that share that input.

A setter can install the field before the initializer is needed. For an `arc`
field, an initializer can also return an existing `Arc<T>`.

The initializer runs outside the input table's lock. If two successful
initializations race for the same entry, both reads return the first value
installed.

An initializer captures external data. During a query read or cache validation,
`check_cancelled()` polls the active operation. For long acquisitions, call it
between chunks and return the error to abandon the capture.

## The host decides when to reacquire

Filesystem notifications are an application concern. On a change, either install
newly captured text in an input batch or forget the old capture:

```rust
# extern crate ajika;
# use ajika::{Database, InputInit, Result};
# fn acquire_text(path: &String, _cx: &dyn InputInit) -> Result<String> {
#     std::fs::read_to_string(path).map_err(ajika::Error::application)
# }
# #[ajika::input(key = String)]
# struct File { #[input(arc, initialize = acquire_text)] text: String }
fn refresh_later(database: &mut Database, path: String) -> Result<()> {
    database.update_inputs(|update| {
        update.forget(File::TEXT, path);
        Ok(())
    })
}
```

The next read can initialize that field again. Use `File::at(path).forget_all(update)`
to forget every field for one path, or `update.forget_all(File::TEXT)` to forget
the text for every path. If absence is a meaningful result, capture it explicitly,
for example as an `Option` value, so queries can depend on absence too.

Prefer one batch for a group of file changes. It defines the next input view and
avoids copying a field table separately for every file.

## A captured view is not a filesystem snapshot

Ajika keeps a captured field stable. It cannot make separate filesystem reads
atomic. If a file changes while an operation discovers several files, the host
must decide whether that combined view is acceptable.

An editor or indexer can attach an immutable source observation to a field instead
of storing text directly. That object may acquire text lazily, but it must keep
the text consistent with the field's stamp. It cannot simply expose “whatever is
on disk now” under an old stamp. Verify acquisition against the expected content,
or reject the operation and capture a new view.

Do not mutate answer-affecting contents of a captured object in place. Ajika only
sees published stamps and reads through its input interface.

## Inspecting inputs from the host

The generated field selector, such as `File::TEXT`, also works with
`Inputs::get`, for example `inputs.get(File::TEXT, &id, &database)`.
You can also pass `&interner` directly, using the same interner that created
the ID. Both host contexts use a no-op cancellation check. `peek` and `entries`
inspect already captured data without triggering acquisition.

Inside a query, use the tracked getter with its `Db`. A host-side peek hidden in
a service would bypass dependency tracking.

The capture and sharing rules live in [input tables](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/input/table.rs#L49). The
[internal input chapter](../internals/inputs.md) follows how snapshots share these
observations while edits remain isolated.
