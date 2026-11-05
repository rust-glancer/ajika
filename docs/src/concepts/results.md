# Values and output handles

There are two useful things a caller can keep: an answer it already received, or
the information needed to ask for an answer later. Ajika represents them differently.

An `Arc<T>` owns a particular answer. An `Output<T>` identifies one result
produced by a query and can acquire that result under the active inputs.

## Keep a value while working with it

Suppose a query produces diagnostics for an editor response. The host may need to
format or send them after `Database::run` finishes. Query calls return `Arc<T>`,
so the host can keep that answer beyond the operation. Repeated callers within
the operation can share the same allocation.

Query declarations normally return an owned `T` through `Result<T>`; the macro
supplies the Arc for callers. A body that already has an Arc can use
[`#[ajika::query(forward)]`](../using/queries.md#return-a-value-or-reuse-an-arc)
to return it directly.

That retained value doesn't update when inputs change. It still reflects the
inputs from when you requested it. This is useful for finishing a response
or comparing old and new answers, but it is not a way to issue more queries
against the old input view.

Keeping the `Arc` also keeps its memory alive. Database retention settings cannot
reclaim values still owned by the application.

## Keep a handle when acquisition should happen later

For later requests, a symbol index may need a way to retrieve names without
keeping every name loaded. It can retain output handles and acquire the values
when a request needs them.

A query can emit additional values with `Db::output`, then return handles to
them. `#[ajika::output]` declares a set of output handles.

For example, one declaration pass may produce a name table and a source-location
table. Completion needs names; navigation needs locations. Separate handles let
those consumers acquire and depend on the relevant table.

`output.value(db)` asks for the value under that operation's inputs. It may share,
load, or compute the result, validating previous work as needed.

| What a caller keeps | After an input edit |
| --- | --- |
| `Arc<T>` | Still contains the previously acquired answer. |
| `Output<T>` | Can ask for that slot's answer under the new inputs. |

Two handles compare equal when they name the same producer and slot. That does
not mean the payloads are unchanged. Reading a handle is the separate observation
of its current payload.

Handles belong to the database that produced them. Like interned IDs,
they should not be mixed across unrelated databases or treated as identifiers
independent of the cache's lifecycle.

## Acquire through the consumer's context

The symbol index can expose names through a view that keeps their handle:

```rust
# extern crate ajika;
use std::sync::Arc;
use ajika::{Db, Output, Result};

struct Symbols {
    names: Output<Vec<String>>,
}

impl Symbols {
    fn names(&self, db: &Db<'_>) -> Result<Arc<Vec<String>>> {
        self.names.value(db)
    }
}
```

Each calling query passes its own context, so its read becomes its own dependency.
A query that instead uses an `Arc` acquired before it started has not made that
tracked read. Helpers and views should keep this distinction clear.

Reading a handle gets the whole value. Selecting one element from the returned
vector still records a dependency on the entire vector. Split the stored result
further only when callers benefit from that smaller boundary.

## Separate loading and execution are different choices

Named outputs are produced together. Splitting names from locations lets
readers load them separately and lets their equality affect consumers separately.
It does not make either half of the producing function lazy.

Use separate query functions when computations should be independently requested.
Use outputs when a shared computation produces several independently consumed
answers. They can be combined: a query can return output handles or keys for
more granular queries.

During production, use the local values being built. Newly emitted handles become
readable when the producer completes, not halfway through its execution.

## A handle doesn't guarantee that the output still exists

Suppose a file's analysis emits a body output for each function. After a function
is deleted, a caller may still hold its body handle, but the producer no longer
emits that slot. Reading the old handle then fails.

Ask for the producer's current output handles or use a membership query to discover
which functions remain. An optional output represents absence by omitting the slot.
An output whose payload is `Option<T>` still exists and can itself return `None`.

Keeping a handle also does not pin a stored record against eviction. Ajika can
recompute an evicted result when requested; the handle does not keep its data loaded.
