# Follow one result

This part follows how Ajika serves queries using the same text analyzer as the
[tutorial](../using/first-queries.md). It reads a file's text, splits it into words,
and counts them:

```text
word_count("notes.txt")
  └─ words("notes.txt")
       └─ File::text("notes.txt")
```

Suppose the text is `hello world`. Ajika must remember both the count, `2`, and
why that count is reusable. Keeping only `2` would not tell it what to do after
the file changes. Keeping every decoded answer and every dependency in memory
would make idle memory grow with the amount of analysis.

The runtime separates the temporary work of answering a request from the stored
evidence needed to answer the next one.

## Three different objects

While `words("notes.txt")` is executing, it has a **frame**: a temporary log of
the reads made by this execution. Its frame records the file's input stamp.
The parent has a separate frame that records the words answer's version.

When the function finishes, the evaluator builds a **record**: a completed
description of this call's dependencies and output versions. Payloads, such as
the vector of words, are stored separately from that record.

During the operation, workers asking for the same query and key share a **memo**:
the working place where they find the record, its validation status, and any
loaded or newly computed values. A memo may begin empty, acquire an old record,
validate it, and eventually hold a replacement answer.

| Object | Question it answers | Lifetime |
| --- | --- | --- |
| [`Frame`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/frame.rs#L32) | What did this execution read and emit? | One execution or parallel branch. |
| [`Record`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/record.rs#L126) | What justified the completed answer? | Retained with reusable query work. |
| [`Memo`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/cache.rs#L26) | What do workers in this operation know about this call? | One operation. |

These are different jobs, not three interchangeable caches.

## One cold operation

The host enters [`Database::run`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/mod.rs#L238). It creates an operation and a root
`Db`. The root has no query of its own, so its reads are not saved as another
dependency graph node.

Calling `word_count` enters the evaluator through the macro-generated provider.
No answer exists, so the evaluator reserves this call and executes the function
with its own frame. Calling `words` repeats that process for the child.

The child reads the captured text, returns a word vector, and receives an output
version. Its record and value become available to other callers in this operation.
The parent records that version, counts the words, and completes in the same way.

Nothing has to reach disk before the parent can use the child. The operation's
working state is the place where completed computation becomes immediately shared.

When the host callback finishes successfully, checkpointing saves eligible changed
records and payloads together. The operation then releases its memos and decoded
values. The backend keeps compact process-local validity and usage information;
the detailed dependency records live in storage. Any `Arc` returned to the host
remains alive for as long as the host retains it.

## The next operation

With the same inputs, the backend can remember that this call was already verified
at the current revision. A new operation retrieves the stored output metadata and
loads the requested count. It doesn't need to check `words` again.

After an input change, that shortcut may no longer apply. The evaluator follows
the old dependencies to see whether their observed answers still match. This is
the red-green check described in [evaluation](evaluation.md).

After a process restart, old process-local revision proofs are gone altogether.
Records and payloads remain useful, but must be checked against the new owner's
captured inputs.

## The implementation boundaries

```text
application: acquire inputs, publish revisions, choose operations
    │
    ▼
declarations: typed wrappers and replay registrations
    │
    ▼
runtime: track reads, validate, compute, share values
    │
    ▼
backend: checkpoint records, retain session work, collect queries
    │
    ▼
ajika_storage: index and payload files, atomic publication, reclamation
```

The macros do not decide whether a query runs. The storage crate does not know
what a dependency means. Those boundaries keep declaration ergonomics, incremental
evaluation, and file management independently understandable.
