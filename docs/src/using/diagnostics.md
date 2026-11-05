# Understanding cost and reuse

Avoiding execution is only one kind of reuse. A request may still walk dependencies,
read metadata, or decode a large stored answer. Ajika exposes these costs separately
so you can choose query boundaries from evidence.

## Start with operation statistics

After `run`, inspect `database.statistics()`. These counters cover the latest
operation and its nested calls, not just the root request.

| Counter | What it helps explain |
| --- | --- |
| `executed` | Query functions that actually ran. |
| `validated` | Records whose saved dependencies were examined. |
| `header_reads`, `metadata_reads` | Disk metadata access for known-valid answers or full records. |
| `decoded`, `bytes_read` | Payload acquisition, including old answers read for equality. |
| `compared`, `unchanged` | Equality checks and recomputations that preserved versions. |
| `resident_hits`, `waits` | Sharing within the operation and contention for unfinished calls. |
| `bytes_written`, `metadata_writes` | Encoded payload work and checkpointed record updates. |
| `session_queries`, `session_bytes` | The retained session working set after collection. |

For example, zero executions with a large `bytes_read` suggests that the request
loads more data than it needs. Many unchanged recomputations show useful equality
boundaries, but may also expose an expensive producer that could be split.

These are not wall-clock timings or exact RSS measurements. `bytes_written`
counts encoded payload bytes, even if the final commit fails. Metadata-only
updates can still happen when it is zero. The
[`Statistics` fields](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/model/mod.rs#L77) specify the individual accounting rules.

## Ask why a particular call ran

Ajika emits diagnostic events through the `tracing` target `ajika::diagnostics`.
Enable it with this filter:

```text
ajika::diagnostics=debug
```

`debug` reports executions and the first dependency that failed validation.
`trace` also reports reuse decisions and individual output outcomes.

An illustrative whitespace edit looks like this, with some fields omitted:

```text
op=17 revision=3 query=analyze call=ab12cd34
  invalidated dependency=File::text reason=input_stamp_changed
  execute reason=dependency_invalid
  output=words result=equal evidence=comparison version=preserved
  output=locations result=unequal evidence=comparison version=advanced
op=17 revision=3 query=word_count call=ef56ab78
  reuse reason=dependencies_valid
```

Every event includes a process-local operation ID and its input revision.
Query and input families have names. `call` is a shortened address hash;
`call_id` retains the full hash for correlation. Named slots include their name;
keyed slots show the name and an opaque key hash. No `Debug` bounds on application
keys or values are required.

A new version doesn't always mean the value changed. The previous value may be
unavailable for comparison; that outcome is `comparison_unavailable`.
Conversely, reconstructing a transient result with still-valid dependencies can
preserve a version by proof without comparing payloads.

Events describe decisions made during evaluation, before checkpointing. Enabling
them does not force additional reads or comparisons. The
[diagnostic implementation](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/diagnostics.rs#L99) only renders evidence the evaluator
already has.

## Separate cache budgets from validity

`Retention::max_idle` controls when unused stored work becomes eligible for
collection. It is not how Ajika detects stale answers. Input stamps and dependency
versions do that on demand, even one millisecond after an edit.

Entry, age, and byte budgets reclaim whole query records with their outputs.
Successful operations collect periodically or when over budget; `collect()`
lets the host request an idle-boundary pass. There is no background timer.

Logical retained bytes, compressed payload bytes, physical files, and allocator
memory are different measurements. `storage_statistics()` exposes physical
storage information for a disk backend. `compact()` performs a full physical
maintenance pass; it can take much longer than regular maintenance.

`set_compression` selects no compression, LZ4, or Zstandard for new payloads.
It does not rewrite existing payloads or alter query equality. Measure with
representative data: serialization, compression, validation, and computation are
different costs.

The [storage chapter](../internals/storage.md) explains where space is used and
how logical eviction becomes physical reclamation.
