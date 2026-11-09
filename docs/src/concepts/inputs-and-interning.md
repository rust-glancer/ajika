# Inputs, interning, and context

A source file has a path, text that changes as the user edits it, and settings
under which it is analyzed. The same file may belong to two projects with different
settings. Keeping these roles separate helps queries reuse work across edits and
projects.

## Interning gives objects short IDs

Plain strings already work as query keys. In a large graph, though, the same file
path may appear in many keys and results. Interning stores that path once and lets
those references carry a compact ID. The rest of the graph can copy, hash, and
compare these IDs as numbers rather than string contents.

The file's path can stay the same while its text changes. We can intern the path
and keep the text in an input addressed by that ID:

```rust
# extern crate ajika;

#[ajika::interned(name = "source-files")]
struct FileId {
    path: String,
}

#[ajika::input(key = FileId)]
struct Source {
    #[input(arc)]
    text: String,
}
```

`FileId::new(&interner, path)` returns the same ID for equal identifying fields
in that interner. Updating the text through `Source::at(file)` keeps the file's ID,
so queries about that file can find their previous answers after an edit.

Fields are compared as supplied. If your application treats two path spellings as
the same file, normalize them before interning.

The host gets the shared `Interner` through `Database::interner()`; queries use
`Db::interner()`. Keep an ID with the database that assigned it: another database
can assign the same number to a different path.

Interned IDs can be part of persistent query keys and results. A durable query
checkpoint saves new ID assignments alongside the query data, and reopening the
same cache restores them. Query collection and `Database::invalidate` preserve
those assignments. A full cache reset can discard both the assignments and the
query data that refers to them; see [Owning the cache](persistence.md).

## Inputs hold the values that change

`FileId` lets queries refer to the same file; the `Source` input supplies its text.
If we also give the file a display-name field, changing that label should not make
a parser reconsider the text. Ajika tracks input fields separately, so a query
depends only on the fields it reads.

When the user edits the file, the host supplies the new text through a generated
setter in `Database::update_inputs`. The batch publishes a new `Inputs` view.
Queries read the text through their `Db`, which records what each answer relied on.

Before reusing an answer, Ajika needs to recognize whether that supplied text has
changed. It records a `Stamp` when the input is supplied or first acquired, then
compares stamps during validation. Default stamps use the encoded value. A custom
stamp can use an existing content fingerprint, but must cover everything queries
can observe. Mutating the text in place without publishing an update can leave
the recorded stamp unchanged and make reused answers stale.

Inputs also describe facts such as existence, membership, and failed lookups.
For example, resolving an optional configuration file depends on its absence
until it appears. Model that absence as a supplied value, perhaps `None`, so
Ajika can notice the transition. Forgetting an input instead asks to acquire it
again; it does not establish that the file is absent.

## Acquire inputs on first use

Some files may never be requested, so the host can use an initializer to load
their text on first use. Later reads in that input view reuse the captured text.
When the host learns that a file has changed, it can supply new text through an
input update, or forget the old value so the next read invokes the initializer
again.

The [lazy-input chapter](../using/lazy-inputs.md) covers initializers, forgetting
captured values, and keeping file acquisition consistent.

## Context can change the answer

Now suppose the same file belongs to two projects using different language
editions. They can share its `FileId` and source text, but a semantic query may
produce different answers in each project. Such a query can take
`(ProjectId, FileId)` and read the settings input for that project. A query that
only needs source text can still take just `FileId`.

Keeping project context in the semantic query's key lets source-only queries
share their work. Giving the file a different ID in each project would separate
those reads too, even when they use exactly the same text.

Read those settings through tracked inputs. `Db::services` is useful for facilities
such as progress reporting, but its values are untracked: changing them does not
invalidate answers.
