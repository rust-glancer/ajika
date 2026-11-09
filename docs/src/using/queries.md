# Choosing query boundaries

A query is a reusable question, not a requirement to wrap every helper function.
Start with work that is expensive, reused, or useful as a boundary where equality
can stop changes. Keep ordinary local computation inside it when no caller benefits
from retaining the intermediate answer.

For our analyzer, a words query is useful because many text edits preserve the
word vector. A query for every byte would add keys, records, and checks while doing
almost no work per call.

## Keys identify questions; reads identify observations

Use a stable key such as a file identity or an owner identity. Read changing content
through inputs or queries. Putting the entire current source text or revision in
the key turns each edit into a new question; Ajika cannot find the old answer to
compare.

Context belongs in the key when it changes the question. If one file is analyzed
under two language editions, a query keyed only by the file would conflate them.
Include the relevant interpretation context, or read it through an input keyed
in a way that identifies that interpretation.

Keys are owned and implement `Clone + Eq + Hash + Send + Sync + 'static + Persist`.
The context is the first argument; every argument after it contributes to the key.
Values are `Send + Sync + 'static`; persistent values also need
`Persist + PartialEq`. These are the ordinary [`Query` bounds](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/model/mod.rs#L169),
whether the implementation is handwritten or generated.

## Return a value or reuse an Arc

Declare a query with `Result<T>` and return an owned `T`. The macro wraps successful
results in `Arc<T>`, so the generated function returns `Result<Arc<T>>` to callers.

Sometimes the answer already belongs to an Arc: another query, a captured input,
or a named output may have produced it. Use `forward` to return that Arc directly,
without copying the payload or allocating another wrapper. The word-count example
includes a query for the fixed `notes.txt` path:

```rust
# extern crate ajika;
# extern crate anyhow;
{{#rustdoc_include ../../../examples/word-count/src/main.rs:forward}}
```

Both `words` and `note_words` return `Result<Arc<Vec<String>>>` to their callers.
`note_words` reuses the allocation returned by `words`.

The `forward` option works on associated queries too.

## Borrow the answer for an operation

Use `returns(ref)` when callers only need to read the answer during the operation.
The body still returns `Result<T>`, and the generated function returns
`Result<&'db T>`.

```rust
# extern crate ajika;
use ajika::{Db, Result};

#[ajika::query(returns(ref))]
fn label(_db: &Db<'_>, id: u32) -> Result<String> {
    Ok(format!("file-{id}"))
}

fn label_text<'db>(db: &Db<'db>, id: u32) -> Result<&'db str> {
    Ok(label(db, id)?.as_str())
}
```

`returns(arc)` spells out the default for callers that need an owned answer.
Both modes work on free and associated queries and can be combined with `forward`.

## Call a query explicitly

A free `#[ajika::query]` function also generates a query type with the same name.
Pass that type to `db.query::<Q>(key)` for an `Arc` or to `db.query_ref::<Q>(key)`
for a borrow. Both calls use the same cached answer and track the same dependency.
They let you choose the return mode at the call site.

```rust
# extern crate ajika;
use std::sync::Arc;
use ajika::{Db, Result};

#[ajika::query(returns(ref))]
fn location(_db: &Db<'_>, file: u32, line: u32) -> Result<String> {
    Ok(format!("{file}:{line}"))
}

fn keep_location(db: &Db<'_>) -> Result<Arc<String>> {
    db.query::<location>((4, 12))
}

fn borrow_location<'db>(db: &Db<'db>) -> Result<&'db String> {
    db.query_ref::<location>((4, 12))
}
```

Use `()` for a query with no key arguments, the argument itself for one, and a
tuple in argument order for several. For a handwritten query, use the type that
implements `Query` and pass its `Query::Key`. For associated queries, call the
generated associated function with its declared return mode.

## Put a query beside a natural owner

An associated query can make a constructor-like computation easier to find:

```rust
# extern crate ajika;
use ajika::{Db, Result};

struct WordIndex(Vec<String>);

#[ajika::query]
impl WordIndex {
    #[ajika::query(transient)]
    fn from_text(_db: &Db<'_>, text: String) -> Result<Self> {
        Ok(Self(text.split_whitespace().map(str::to_owned).collect()))
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}
```

This small example takes text directly as an owned key to demonstrate the signature.
A file-backed index would normally key by the file and read its text through an input.

Mark the concrete inherent impl and each query function. Ordinary methods remain
ordinary methods, and `Self` works in signatures and bodies. Query functions have
no receiver. Generic and trait impls are not supported.

The owner also becomes part of the query's default name, so `WordIndex::from_text`
gets a name such as `module::WordIndex::from_text`. The
[`query` attribute documentation](https://github.com/rust-glancer/ajika/blob/main/crates/ajika-macros/src/lib.rs#L29)
lists the available overrides and other declaration options.

## Choose what survives an operation

`#[ajika::query]` retains its result according to the database backend.
`#[ajika::query(transient)]` releases the primary result when the operation ends.
The latter is useful for an intermediate that should not need serialization or equality.

A transient query's dependency record can still be retained. A stored parent may
validate through it without asking for its value. If someone does need that value
again, its function runs to reconstruct it.

When the old dependencies are still valid, that reconstruction can preserve the
known output version. If its dependencies changed, there is no `PartialEq` comparison
available for the transient result, so Ajika conservatively assigns a fresh version.
Use a persistent semantic result when equality is an important invalidation boundary.

| Decision | Controls |
| --- | --- |
| Persistent or transient query | Whether its primary payload can be retained. |
| Input `Lifetime` | How long work derived from that observation may be retained. |
| Memory or disk database | How eligible persistent-policy results are stored. |

A memory database does not make transient query values survive operations.
A serializable result depending on session input does not become durable.

## Equality is part of the contract

After recomputation, `PartialEq` decides whether consumers may keep using the
previous answer. It must include every property they can observe. Ajika may retain
the old serialized bytes when values compare equal.

If locations should change without invalidating semantic consumers, split them
into [outputs](outputs.md); do not hide observable locations from equality.
Likewise, answer-affecting reads through services or mutable global state would
break determinism. Host services are for facilities such as progress reporting,
not an alternative input channel.

Keep output granularity proportional to how callers read data. Reading an
`Output<T>` decodes the whole `T`, not selected fields. An operation also retains
its touched results until it finishes. A finer query boundary can reduce the data
requested; it does not eliminate the cost of a large operation that requests everything.
