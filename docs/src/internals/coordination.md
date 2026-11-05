# Coordinating workers

Two workers may reach `words("notes.txt")` at the same time. They should share
both expensive computation and less visible work such as validating dependencies
or decoding a stored answer.

The memo holds the answer. A separate **claim** decides who may do unfinished
work on that call.

## Reserve work, not a mutex

[`Coordination::claim`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/coordination.rs#L224) reserves a call ID. If another worker owns it,
the caller waits. When the owner releases the claim, the waiter checks the memo
again and can use the completed answer.

The claim guard does not keep the coordinator's mutex locked. Provider code,
recursive queries, storage reads, and value comparisons run outside it. The memo's
state mutex likewise protects short reads and publication, not the whole
computation. Unrelated calls can make progress concurrently.

A claim covers validation and loading as well as execution. A warm database can
therefore avoid duplicate decoding even when no query function needs to run.

## Track active calls to detect cycles

Thread-local recursion checks are insufficient when one worker owns A and another
owns B:

```text
worker 1: owns A, requests B
worker 2: owns B, requests A
```

Both would wait forever unless the runtime sees the relationship across workers.
The coordinator keeps an active parent-to-child call graph. Before adding an edge
A → B, it checks whether B already reaches A.

This graph includes running nested calls and calls waiting for claims. It is
different from the saved dependency graph: it describes unfinished work, not
observations used by completed answers.

Parallel branches can produce the same edge more than once. Edges are reference
counted so one branch completing does not erase another branch's relationship.
When the graph reveals a cycle, [`QueryCycle`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/coordination.rs#L110) reports the calls in order,
including the repeated first call that closes the chain, such as A → B → A.

## Failure must wake waiters

The coordinator retains the operation's first failure and notifies waiting workers.
They receive a shared error that preserves the original error chain and application
context.

If provider code panics before publishing an answer, the waiting worker still
needs to wake up. The [claim guard](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/coordination.rs#L303) releases ownership and its call edge during
unwinding, and records an operation failure. Waiters then receive an error instead
of retrying the abandoned computation. Cancellation similarly wakes waiters and
is checked at query boundaries.

Cancellation is cooperative. A worker in a long loop or blocking external call
still needs to return or check the token. The coordinator does not suspend or kill
application threads.

## Bounded parallel batches

[`Db::parallel_map`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/parallel.rs#L60) uses scoped threads. The caller participates,
while extra threads consume permits from the operation's shared worker budget.
A nested batch with no spare permits runs locally instead of creating more threads.

Workers claim successive item indices for load balancing. Each saves its result
and frame at the input index; joining then preserves result and dependency order.
The helper also carries the caller's tracing dispatcher into its workers.

The scoped-thread model keeps each query's owner able to finish its call stack.
In a work-stealing executor, a worker waiting for a batch might start an unrelated
task. If that task asks for a query owned by the suspended stack beneath it, it
can block the very worker needed to finish that query. This can happen even when
the active call graph has no cycle, so the coordinator can't detect this scheduler
deadlock.
Integrating another executor requires preserving progress for query owners too.
