# Working with unsaved edits

An editor needs to reuse work between keystrokes without replacing saved results
on disk. Mark the edited observations as session inputs. Queries still use the
same functions and keys; their input reads determine where results may be kept.

## Lifetime follows the observations

Each field observation has one of three lifetimes:

| Lifetime | Derived work may survive |
| --- | --- |
| `Operation` | Only the operation that computed it. |
| `Session` | Later operations in this database owner, within a memory budget. |
| `Persistent` | Later operations and, on a disk backend, reopening. |

A query inherits the shortest lifetime it reads, including through child queries.
If a saved project depends on one unsaved file, affected work becomes session work;
unaffected saved queries remain durable.

Lifetime does not describe how often data changes. That is the separate
[durability](inputs.md#durability-is-a-shortcut-not-permanence) classification.

## Keep the saved view and override one field

The [editor-session example](https://github.com/rust-glancer/ajika/blob/main/examples/editor-session/src/main.rs)
stores text together with its retention policy:

```rust
# extern crate ajika;
# extern crate anyhow;
# extern crate tempfile;
{{#rustdoc_include ../../../examples/editor-session/src/main.rs:input}}
```

The custom stamp identifies the text; the lifetime callback supplies its policy.
Changing policy is still an input change even if the stamp stays equal.

After computing the saved file, the application creates a sparse input overlay:

```rust
# extern crate ajika;
# extern crate anyhow;
# extern crate tempfile;
{{#rustdoc_include ../../../examples/editor-session/src/main.rs:overlay}}
```

The overlay refers to the saved base and stores only its overrides. It does not
copy all saved sources. Missing fields can still be acquired through the base.
The application retains the saved view so it can discard or save editor changes
later.

The first operation computes the edited count. The second reuses it directly
from the session store. Neither writes that edited payload to disk.

This is one installed input view and one set of latest session results per owner.
It is not a facility for querying several historical revisions concurrently.

## Saving can reuse the same computation

After the host successfully saves those exact bytes, it publishes the same content
as persistent:

```rust
# extern crate ajika;
# extern crate anyhow;
# extern crate tempfile;
{{#rustdoc_include ../../../examples/editor-session/src/main.rs:save}}
```

The text stamp matches. Ajika refreshes the derived lifetime and can promote the
retained result to durable storage without executing the query again. This reuse
depends on the session result still being retained; eviction may require computation.

If an edit is discarded, install the saved input view instead. That is a new
revision, not a rewind of the runtime. Validation decides which saved answers
match it.

## Budget the session separately

`Database::set_session_retention` controls retained session records and changed
payloads. The default budget is 16,384 records, 64 MiB of estimated encoded data,
and five minutes since last use. These are cache limits, not a measurement of
allocator memory.

The session store can refer back to an equal durable payload rather than keep a
second decoded copy. Changed payloads remain typed values in memory. Transient
query results still expire with the operation.

`clear_session` drops retained session query work. It does not close editor
buffers or replace input overlays; the host owns those actions. Keeping many
returned `Arc` values alive also keeps their allocations alive independently
of these budgets.

[Checkpointing](../internals/checkpoints.md) explains how Ajika separates
session overrides from the durable cache.
