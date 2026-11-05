# Interning objects

Interning gives equal immutable objects the same compact ID. An interned declaration
describes the fields that identify an object; an `Interner` assigns their IDs.
This keeps query keys small when many calls refer to the same object.

For example, a file path can identify a file while its contents change:

```rust
# extern crate ajika;
use ajika::{Database, Result};

#[ajika::interned(namespace = "source-files")]
struct FileId {
    path: String,
}

fn intern_file(database: &Database, canonical_path: String) -> Result<FileId> {
    let interner = database.interner();
    let first = FileId::new(&interner, canonical_path.clone())?;
    let second = FileId::new(&interner, canonical_path)?;
    assert_eq!(first, second);
    let path = first.path(&interner)?;
    assert!(!path.is_empty());
    Ok(first)
}
```

The generated ID is copyable, comparable, hashable, and serializable. You do not
convert integers or manually encode keys. Each declaration has one numbering space
per database. The explicit namespace is optional; its default includes the module
and type name.

## Choose what to intern

Intern the immutable facts that identify an entity. If you identify a file by its
canonical path, canonicalize it before calling `new`.
Ajika has no filesystem policy: two differently encoded paths are different keys.

If package or language context belongs to interpretation rather than identity,
keep it in the semantic query's key. If it really distinguishes the entity itself,
put it in the interned fields.

Editable text is usually an input value, not an interned field. Interning a new
complete text on every edit would retain a growing history of interned objects.

## Use the database's interner

`database.interner()` gives the host access to the database's assignments,
including any restored from disk. The host can use this handle while constructing
inputs, and queries can reach the same interner through `db.interner()`. Clones
share assignments, so two workers interning the same path still get the same ID.

An ID doesn't record which database created it. Looking it up through an unrelated
interner can resolve a different entity. Keep the owner relationship in the
application's design.

Field lookup decodes the immutable key and returns an owned value. It records no
query dependency because that assignment does not change. File existence, contents,
and membership are separate observations and belong in tracked inputs.

## What survives

Assignments are never recycled within a live interner. Query collection and
`Database::invalidate` preserve them. New assignments become durable alongside
a successful durable query checkpoint; merely allocating an ID or running an
operation with no durable changes is not a persistence barrier.

Reopening a compatible cache restores saved assignments before handing out the
interner. A full reset can discard them. This includes epoch changes and the
interner size limit checked at open, so IDs must not become an application's
authoritative identifiers outside the cache lifecycle.

See [`Interner`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/interner/mod.rs#L23) for allocation and checkpointing, and
[storage reclamation](../internals/storage.md) for the distinction between query
collection and identity reset.
