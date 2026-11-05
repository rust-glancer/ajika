# Parallel work, errors, and cancellation

An operation is a useful unit of application work: an indexing pass, one editor
request, or a batch of related requests. Queries within it share decoded and
computed values. When it finishes, Ajika checkpoints eligible work and releases
that working set.

Choose the boundary with both reuse and peak memory in mind. Starting an operation
for every tiny helper loses sharing. Keeping one operation open indefinitely keeps
everything it touched alive.

## Run independent work together

Query calls are synchronous. A parent waits for the children it calls, and two
workers requesting the same call share its computation.

Use `Db::parallel_map` for a batch of independent work:

```rust
# extern crate ajika;
use ajika::{Db, Result};

# #[ajika::input(key = String)]
# struct File { text: String }
#[ajika::query]
fn word_count(db: &Db<'_>, path: String) -> Result<usize> {
    Ok(File::at(path).text(db)?.split_whitespace().count())
}

#[ajika::query]
fn total_words(db: &Db<'_>, files: Vec<String>) -> Result<usize> {
    let counts = db.parallel_map(&files, |db, path| word_count(db, path.clone()))?;
    Ok(counts.iter().map(|count| **count).sum())
}
```

Each callback receives a context for the same operation. Its reads become reads
of `total_words`; there is no extra query declaration for a callback.
Results keep input order even when work finishes out of order.

`Database::set_parallelism` limits workers, including the calling thread.
Nested batches share that budget. All scoped workers finish before
`parallel_map` returns, including after failure.

The supported helper uses scoped threads that keep making progress while a query
waits. Arbitrary work-stealing pools can suspend a query owner beneath a task
waiting for that owner. Do not assume that substituting a Rayon batch preserves
this scheduling property.

The database owner itself is exclusive: `run` and input replacement require
`&mut Database`. To react to an edit during work, cancel or finish the operation,
then install the next inputs.

## Keep host services separate from semantic inputs

The second argument to `Database::memory`, or the services argument to `open`,
becomes available through `db.services()`. It may hold progress reporting,
tracing facilities, or other host integration.

For a `Services` type, queries take `&Db<'_, Services>`. Services are untracked:
changing them does not invalidate queries. Values that affect an answer belong
in inputs, even if the application also stores them elsewhere.

Queries must not rely on execution for required side effects. A cache hit skips
the function. For example, progress reporting may describe work that actually
runs, but saving a user document cannot be hidden inside a query.

## Errors abandon an operation's pending work

A query error is an operation failure. It prevents new work from being
checkpointed and wakes workers waiting for that work. Catching a child query's
error inside another query does not make the operation healthy again.

Represent expected domain problems—such as a syntax error with useful partial
analysis—as successful result data. Reserve `Err` for work the operation cannot
complete. A host can also return an error from the `run` callback to reject a
candidate before checkpointing.

No fixed-point recovery is provided for query cycles. Ajika returns a
`QueryCycle` containing a closed chain of calls, including cross-worker cycles.
Application error context is preserved:

```rust
# extern crate ajika;
# extern crate anyhow;
fn report_cycle(error: &anyhow::Error) {
    if let Some(cycle) = error.chain()
        .find_map(|cause| cause.downcast_ref::<ajika::QueryCycle>())
    {
        eprintln!("{cycle}");
    }
}
```

Recursive domain algorithms can still live inside a query. A recursive call to
another key is also fine when it terminates; a call graph that returns to an
unfinished key is the cycle that cannot complete.

## Cancellation is cooperative

Obtain a `Cancellation` handle from `database.cancellation()` before starting
work. Another thread can call `cancel()`. Query boundaries and parallel workers
check it, and long local loops should call `db.check_cancelled()?` themselves.

Cancellation does not forcibly interrupt arbitrary Rust code or an I/O syscall.
The owner issues a fresh token after the operation or input replacement; retain
the token for the work you mean to cancel, not for all future operations.

A failed operation releases its working set and claims. A cache checkpoint may
already have committed before a late cancellation or maintenance error, so the
host still decides whether to publish the response. See the
[operation implementation](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/mod.rs#L238) for the exact boundary.
