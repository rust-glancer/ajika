# Where results live

Reusing an answer does not require keeping its decoded Rust value in memory.
Ajika can remember that an answer is valid, load its value when requested, and
release that value when the operation ends.

It helps to make two separate decisions: whether an answer is still correct,
and whether retaining its data is worth the memory or storage cost.

## First choose what a query produces

Consider parsing a file and extracting its signatures. The signatures are a
compact answer worth reusing across requests; the parse tree may be large or
awkward to serialize.

A default `#[ajika::query]` suits the signatures. Its result must support serialization
and equality, so Ajika can retain it between operations and compare it after edits.
The parse query can use `#[ajika::query(transient)]` instead. Its value doesn't need
to support serialization or equality, and Ajika only keeps it for the current
operation.

Ajika can still retain the parse query's dependency information. If the source
has not changed, saved signatures can remain valid without rebuilding the tree.
Requesting the tree itself in a later operation requires reconstruction.

If the source changes, checking the signatures requires rebuilding the tree and
extracting them again. The old tree is unavailable for comparison, but the new
signatures can still be compared with the saved ones. When signatures stay equal,
queries that use them can keep their answers.

## Then decide where observations allow work to survive

The same parse and signature queries can analyze saved files or unsaved editor
buffers. Draft results should be reusable across editor requests while leaving
the saved disk answers available. Marking the draft input as `Session` gives
its derived results that restriction.

An input observation's `Lifetime` sets this limit:

| Lifetime | Suitable use | Derived work may survive |
| --- | --- | --- |
| `Operation` | An observation valid only for this request. | Within the current operation. |
| `Session` | An unsaved editor buffer. | Later operations in this owner, within a memory budget. |
| `Persistent` | Saved source or captured configuration. | Later operations and, with disk storage, restarts. |

Restrictions flow through child queries. A result depending on one session input
cannot overwrite the saved disk result, even if its other inputs are persistent.
All outputs of one producer share that restriction.

The host can keep the saved view and create an `Inputs::overlay` for a draft.
Only the edited text needs a session override; unchanged inputs remain shared.
When the host saves that same text and publishes it with persistent lifetime,
Ajika can promote results that are still in the session store without recomputing
them. The [unsaved-edits chapter](../using/sessions.md) shows this lifecycle.

`Lifetime` is an upper limit on retention; cache budgets can evict a result
sooner. A transient query still releases its primary value at operation end.

## The backend supplies the storage medium

`Database::open` keeps eligible results on disk. `Database::memory` keeps
eligible values as Rust objects for the owner's lifetime, subject to retention.
Both expose the same query interface.

These choices combine as follows:

| Configuration | Between operations |
| --- | --- |
| Disk, default query, persistent inputs | Stored payload can be reloaded; decoded working values are released. |
| Disk, default query, session input | Retained changed results stay in the bounded session store. |
| Disk, transient query, persistent inputs | Dependency information can survive; the primary value is reconstructed when needed. |
| Memory, default query, persistent inputs | Eligible typed values remain in memory. |

The word “persistent” in a query policy means eligible for backend retention.
Choosing a memory database does not write files, and choosing a disk database
does not make session inputs durable.

## Durability answers a different question

Source text may change on every keystroke while compiler settings stay the same
for hours. `Durability` lets the host describe that difference. With settings
marked `High` and source text marked `Low`, a source edit doesn't force Ajika to
recheck queries that depend only on settings.

This reduces validation work; `Lifetime` and the backend still determine where
results can survive. Use the default low class until the application has a useful
distinction to make.

## Retention controls cost, not correctness

Over time, an editor may request data for many files it no longer needs.
`Retention` places age, count, and byte budgets on that retained work.
`set_session_retention` controls the session store separately. These are cache
budgets: eviction can require recomputation later, but does not make answers stale.
The age limit is not a delay before detecting input changes.

`clear_session` lets the host drop derived session results as part of closing
its drafts. The editor's buffers and input views remain owned by the host, which
can close or replace them as part of the same cleanup.

Those input views and any query results the host keeps in `Arc`s have their own
lifetimes. They stay alive for as long as the host keeps them, even after an
operation ends or a retention pass trims the query cache. The
[cost and reuse guide](../using/diagnostics.md) explains cache statistics and their
relationship to process memory.
