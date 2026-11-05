# Queries and observable change

A query defines a question that remains useful to ask after inputs change.
The function describes how to answer it; calling the function asks Ajika for the
current answer. Calling and executing are different events.

That distinction is why query code should describe a computation, rather than a
step that must happen every time a request arrives.

## Keep the question stable

The query family and its key identify the question. All arguments after the
`Db` contribute to the key. For a declaration query, a file and interpretation
context are plausible keys. The current source revision usually is not.

Including a revision or entire changing input in the key creates a new call after
each change. That can be useful when content really is the computation's identity,
but it gives up comparing successive answers about the same entity.

Read changing state through inputs and child queries instead. Then Ajika knows
both which previous answer to consider and which observations could make it stale.

## A dependency means “my answer relied on this”

If a completion query reads an import table, it depends on the import table's
answer. It does not automatically depend directly on every source file used to
build that table. A source edit that leaves the table equal can leave completion
reusable.

Relationships follow the reads that actually occur, including conditional ones.
You do not need to predict a query's full dependency set in its declaration.

A view built before the completion query runs might already hold an
`Arc<ImportTable>`. Reading that Arc through a helper does not tell Ajika that the
completion query depends on the table. If the view keeps a query key or output
handle instead, the helper can acquire the table through the caller's `Db`.
Each query using the view then records its own read.

That tracked read depends on the whole table, even if the caller only uses one
field. A smaller query result or [named output](results.md) can provide a narrower
boundary when callers benefit from it.

## Equality describes interchangeable answers

Persistent query results use `PartialEq` to decide whether dependent work can
keep its old answer. Equal results must be interchangeable for their consumers.
Ajika may keep the old stored bytes instead of saving the newly computed value.

Suppose declarations contain both signatures and source locations. Defining
equality to ignore locations is wrong if callers can read those locations.
Separate signatures and locations into independently comparable results instead.

The same principle applies to custom input stamps: they describe equivalence of
observations, not just the fields an author happens to find important.

A query can reach the same answer through different reads. For example, it can
select a different child after an edit and still return the same value. Future
reuse will follow the new reads, while callers may continue reusing the equal answer.

## Choose boundaries that callers can benefit from

A good query boundary often serves one of three purposes: sharing expensive work,
letting callers request a useful subset, or stopping change propagation at an
answer that often stays equal.

A query per trivial helper can cost more in metadata and acquisition than it saves.
A single project-wide query can force small requests to load and compare far more
than they use. Look at the questions features ask and the kinds of edits that
should leave their answers unchanged.

Named outputs help when one computation naturally produces several independently
used values. Separate queries help when the computations themselves should run
independently. Those are different ways to improve granularity.

A helper that only assembles a small value may be clearer as an ordinary function.
Use a query declaration when callers benefit from remembering its answer.

## Execution must remain optional

The same observations must yield an equivalent answer. Time, random values, or
mutable services must not secretly determine the result. Required side effects,
such as saving a document, belong in host control flow because a reused query
skips execution.

Cycles between unfinished calls return `QueryCycle`; Ajika does not solve them
as fixed points. A recursive domain algorithm can run inside one query, and
recursive queries over different keys can work when they terminate.
