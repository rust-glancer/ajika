# Checkpointing an operation

During evaluation, completed answers are visible to other queries in the operation.
They are not yet part of the retained backend state. This lets a parent use a new
child immediately while still allowing the host to reject the whole operation.

[`Database::run`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/mod.rs#L238) starts checkpointing only after the callback returns
successfully, scoped workers have finished, and cancellation has been checked.

## Decide what can survive

[`Backend::checkpoint`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/backend/mod.rs#L372) gathers memos marked dirty: they have record or payload
changes for checkpointing to handle. It prepares replacements for retained work.
The query's inherited lifetime determines where Ajika can keep the record and
its values:

```text
operation lifetime  → no retained replacement
session lifetime    → bounded in-memory override
persistent lifetime → durable backend
```

The destination applies to the dependency record as well as eligible values.
A transient primary result has no retained payload, but its dependency record
can still survive in the session store or durable backend. Its serializable named
outputs can survive there too.

If nothing is dirty, there is no checkpoint work. In particular, allocating an
interned ID by itself does not force a durable transaction; pending assignments
are included when query work is durably checkpointed.

## Reuse durable payloads where versions match

For a disk database, the **durable baseline** is the record and payloads already
saved on disk. Checkpointing compares the completed output metadata with that
baseline. An output that kept its version and already has stored bytes can retain
that location. A removed slot loses its location. New or changed persistent
values need encoding.

In the whitespace edit, `words` kept W1, so its saved vector can stay where it is.
Its record must still change: the function now depends on text stamp S2 instead
of S1. Leaving S1 in the record would make a later dependency check rediscover the
same change and recompute `words` again. No payload bytes need rewriting for this change.

Many outputs may change in one operation. [`PendingValue::write_all`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/backend/encoding.rs#L28) prepares
bounded batches so checkpointing does not keep an encoded copy of every changed
value at once. Workers serialize and compress a batch in parallel; the caller
passes its buffers to the storage writer and releases them before the next batch.
Limits cover raw size and value count. One oversized value runs alone and still
requires a complete buffer.

The writer publishes records, output locations, and new interner assignments in
one storage commit. Opening the writer at this stage keeps query execution outside
the durable transaction.

## Session work overlays the saved baseline

A session record contains the producer's complete output metadata and dependency
list. Its changed persistent-policy payloads remain typed values in memory.
Equal durable payloads can be read from their saved locations instead of being
duplicated in the session store.

Consider the body outputs A and B from the [output chapter](outputs.md). If an
unsaved edit deletes B, the session record must list only A. Falling through to
the saved output metadata would incorrectly make B visible again. A surviving
slot such as A can still use its saved payload when the versions match.

An operation containing only session changes does not open a durable writer.
If the host later saves that exact draft, it publishes the same input content
with persistent lifetime. Validation can reuse the session answer, and
checkpointing promotes its retained data without rerunning the query.

The typed memory backend uses the same logical decisions, but keeps eligible
Rust values directly rather than serializing them.

## Publish bookkeeping after storage succeeds

Only after a successful checkpoint does the backend replace retained session
work and update its usage inventory. The owner then remembers verified revisions
for accepted records. Otherwise, a later operation could skip validation based
on a replacement that existed only in the failed operation's working memos.

The working memos leave with the operation on both success and failure.
Retained host `Arc` values are outside that cleanup.

Ordinary checkpoint errors can leave storage status uncertain. The backend then
rejects further work until the host reopens it or successfully invalidates it.
Cancellation and a storage-capacity refusal have their own handling; they are not
automatically treated as an unusable cache.

## Cache commit and host acceptance differ

There is a cancellation check after commit, and maintenance can also return an
error afterward. Consequently, `run` can fail even though a valid cache checkpoint
was published.

The host should not infer acceptance of a project generation or editor response
from the mere existence of those files. Stored results retain their actual input
observations, so they can be validated later, regardless of whether the host used
the response.

After the working set is released, routine retention and physical maintenance
can run. That ordering keeps maintenance from carrying the operation's decoded
query values into an otherwise idle period.
