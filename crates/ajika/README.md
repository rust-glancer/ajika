# Ajika

Ajika is a synchronous incremental query library with persistent storage. It
tracks the inputs and other query answers each function reads, so unchanged
answers can be reused after edits and restarts.

Calls share query values within an operation started by [`Database::run`]. The
disk backend saves reusable results and releases its working data when that
operation ends.

[The Ajika Book](https://rust-glancer.github.io/ajika/) provides a tutorial,
design guidance, and a walkthrough of the implementation.

## Example

Declare an input and two queries, then supply text and ask for its word count:

```rust
use std::time::Duration;
use ajika::{Database, Db, Inputs, Result, Retention};

#[ajika::input(key = String)]
struct File {
    #[input(arc)]
    text: String,
}

#[ajika::query]
fn words(db: &Db<'_>, path: String) -> Result<Vec<String>> {
    let text = File::at(path).text(db)?;
    Ok(text.split_whitespace().map(str::to_owned).collect())
}

#[ajika::query]
fn word_count(db: &Db<'_>, path: String) -> Result<usize> {
    Ok(words(db, path)?.len())
}

# fn main() -> anyhow::Result<()> {
let limits = Retention {
    max_entries: 10_000,
    max_bytes: 256 * 1024 * 1024,
    max_idle: Duration::from_secs(24 * 60 * 60),
};
// This example needs no host services.
let mut database = Database::memory(Inputs::default(), (), limits);
let file = File::at("notes.txt".into());
database.update_inputs(|update| file.set_text(update, "hello world".to_owned()))?;
let count = database.run(|db| word_count(db, "notes.txt".into()))?;
assert_eq!(*count, 2);

database.update_inputs(|update| file.set_text(update, "hello    world".to_owned()))?;
let count = database.run(|db| word_count(db, "notes.txt".into()))?;
assert_eq!(*count, 2);
assert_eq!(database.statistics().executed, 1); // Only words ran.
# Ok(())
# }
```

The whitespace edit reruns `words`, whose vector still compares equal, so
`word_count` reuses its answer. Query bodies declare `Result<T>`; the
[`query`](macro@query) macro returns `Result<Arc<T>>` to callers.

## Choose storage

The example uses [`Database::memory`] to keep reusable results in RAM. With
[`Database::open`], results can survive process restarts on disk. The application
supplies current inputs on startup, and Ajika validates saved answers against
them when requested. The book's [persistence guide](https://rust-glancer.github.io/ajika/using/persistence.html)
walks through opening and reopening a cache.

## Find the API you need

| Task | API |
| --- | --- |
| Declare queries, including associated functions and forwarding | [`query`](macro@query), [`Query`] |
| Supply, update, or lazily load inputs | [`input`](macro@input), [`Inputs`], [`InputUpdate`] |
| Run work against the current inputs | [`Database`], [`Db`] |
| Choose result lifetimes and cache limits | [`Storage`], [`Lifetime`], [`Retention`] |
| Publish separately readable outputs | [`output`](macro@output), [`Output`] |
| Intern immutable values | [`interned`](macro@interned), [`Interner`] |
| Run parallel work or handle cancellation | [`Db::parallel_map`], [`Cancellation`], [`QueryCycle`] |
| Handle query failures and cancellation | [`Error`], [`ErrorKind`], [`ResultExt`] |
| Inspect and maintain the cache | [`Statistics`], [`Database::collect`], [`Database::compact`] |

For guidance on query boundaries and reuse, see the book's
[conceptual reference](https://rust-glancer.github.io/ajika/concepts/overview.html).
The [diagnostics guide](https://rust-glancer.github.io/ajika/using/diagnostics.html)
explains statistics and tracing events.
