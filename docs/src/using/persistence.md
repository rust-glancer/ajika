# Keeping results across restarts

The first example used a memory database. Switching to disk storage changes the
owner's construction, not the query functions or their callers.

The [persistence example](https://github.com/rust-glancer/ajika/blob/main/examples/persistence/src/main.rs)
opens a temporary cache, computes a word count, closes it, then opens it again.
Run it with `cargo run -p ajika-persistence`.

## Open, compute, close

```rust
# extern crate ajika;
# extern crate anyhow;
# extern crate tempfile;
{{#rustdoc_include ../../../examples/persistence/src/main.rs:first-open}}
```

`path` names a directory the application has chosen for rebuildable query data.
The example uses a temporary directory so running it leaves no permanent cache.
The operation checkpoints successful work before returning. No per-query save
call is needed, and dropping the owner releases its directory lock.

Only one owner may open that storage directory at a time. Parallel query calls
share one owner through an operation; opening another database for each worker is
not the concurrency model.

## Reopening still needs fresh inputs

```rust
# extern crate ajika;
# extern crate anyhow;
# extern crate tempfile;
{{#rustdoc_include ../../../examples/persistence/src/main.rs:reopen}}
```

The cache contains the answer and the observations that justified it. It does not
contain the application's input snapshot. The application supplies fresh inputs
on startup, either eagerly or through [lazy acquisition](lazy-inputs.md).

This new owner has no proof that the stored answer matches its inputs. It follows
the count's dependency on words, checks the words' saved input stamp, and finds the
same text. It can then load the count without executing either function or decoding
the words.

## Valid does not mean resident

Ask again under the same inputs:

```rust
# extern crate ajika;
# extern crate anyhow;
# extern crate tempfile;
{{#rustdoc_include ../../../examples/persistence/src/main.rs:later-operation}}
```

No dependency walk is needed: this owner already verified the answer for its current
revision. But the previous operation released its decoded working set, so this one
decodes the requested value again.

| When a query is requested | Typical work |
| --- | --- |
| Again within the same operation | Share its resident `Arc`. |
| In a later operation, with the same inputs | Recover its output description and load the value. |
| After a relevant input change | Validate dependencies and execute where needed. |
| After reopening | Validate against fresh inputs before trusting stored results. |

Validation can check a child's output version without loading its payload. It does
not imply decoding the whole reachable graph.

## Values and schema changes

Persistent query values need `PartialEq` and Ajika's `Persist` serialization bound.
For your own value types, deriving `wincode::SchemaRead` and `SchemaWrite` normally
supplies `Persist`. Query keys must also be serializable. All query keys and values
must meet the thread-safety and ownership bounds described in
[query boundaries](queries.md).

The `"word-count-v1"` argument is an application **epoch**. Change it when a new
query implementation, key encoding, or result schema makes old stored work unsafe
to reuse. Ajika tracks input changes; it cannot discover that you edited a function's
Rust code. A different epoch discards the old cache on opening rather than migrating it.

## Recovering a cache

A run that encounters a damaged value returns a storage error and leaves repair
pending. The next uncancelled `run` on the same database attempts repair before
invoking its callback. If repair fails, the caller receives that error.
Maintenance can also discover damage after a successful run, so
`database.recovery_pending()` reports whether repair is due between runs.

Live repair keeps the installed inputs and interner, as does
`database.invalidate()` when the host chooses to clear saved work. If
corruption forces Ajika to discard results, it logs a warning and recomputes
them on demand. A new process gets its identity assignments from disk, so it
starts with a fresh interner if the saved identity table is lost.

## A checkpoint is not application acceptance

The host should reject incoherent input capture before returning successfully from
`run`. Ajika can then abandon the operation's new query results.

The host still owns publication of an editor response or project generation.
Cancellation can arrive after a checkpoint has committed. Stored results
remain associated with the observations they actually used. See
[`Database::run`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/mod.rs#L238) and [checkpointing](../internals/checkpoints.md).
