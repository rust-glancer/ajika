# Operations and application state

The host owns a long-lived `Database`; an operation is one bounded period of
work on its installed inputs. This separates the lifetime of the application
from the lifetime of the data needed to answer a request.

`Database::run` is the entrypoint. Its callback receives a `Db` that can call
queries, read inputs, and share work with other calls in that operation.

## Choose a useful unit of work

An editor request is often a natural operation. Several related features can also
share one operation when they should use the same inputs and loaded values.
An indexing pass may ask many queries in one operation.

Large operations improve sharing, but retain more touched data until they finish.
Very small operations release memory earlier, but later operations may need to
load the same values again. Start from the application's actual unit of work,
then measure acquisition and peak memory before changing that boundary.

An operation is also the checkpoint boundary. Successful query results are usable
by other queries immediately; eligible changed work is saved after the callback
finishes successfully. Query authors do not manage a separate commit for each call.

## Operations and revisions are not the same thing

```text
publish input view A
    run request 1
    run request 2
publish input view B
    run request 3
```

Requests 1 and 2 can use the same input revision. An operation does not itself
advance the revision. Publishing changed input stamps or policies does; publishing
equivalent observations does not.

The revision is Ajika's bookkeeping. A document version or project generation
can have different meaning in the application. The host remains responsible for
deciding whether a result belongs to the generation it wants to expose.

Replacing inputs and running operations require exclusive access to the database
owner. There is one installed input view at a time. Cloned `Inputs` can describe
proposed edits or previously used snapshots, but queries run against the installed view.

## Publish a view, then decide whether to accept its results

An editor may want to analyze proposed edits before accepting them, while keeping
its last accepted inputs available. `Inputs::edit` prepares that candidate view
without switching the database to it; `replace_inputs` installs it for queries.
For ordinary updates, `update_inputs` prepares and publishes the edit batch in one call.

Once installed, that candidate remains the database's input view even if the
following operation fails. The host can restore its accepted view or install a
new candidate before trying again.

Returning an error from the `run` callback abandons pending derived work before
checkpointing. A late cancellation or maintenance error can still occur
after cache data was committed. A failed `run` therefore does not mean “the
storage directory is unchanged.”

This is safe for reusable derived work because later requests validate it against
their inputs. Publishing an editor response or accepting a project generation
remains a separate application decision.

## Parallel work stays inside the operation

Use `Db::parallel_map` to request independent work concurrently. Its callbacks
share inputs and query results, and their reads belong to the calling query.
`Database::set_parallelism` bounds its workers across nested batches.

The helper joins workers before returning. It does not create independent input
revisions or detached tasks. Ordinary child query calls remain synchronous.
Do not assume that an arbitrary work-stealing executor can replace this helper:
its scheduling must keep a query's owner able to finish while another call waits.

## Failures and cancellation stop the work

Use successful result data for expected domain problems, such as diagnostics
for broken source code. A query returning `Err` fails the operation; catching
that error inside a parent does not permit a partial successful checkpoint.

For cancellation, obtain a fresh `Cancellation` token from the owner for each
operation. Another thread can signal it. Query boundaries notice cancellation;
long local work should call `Db::check_cancelled` too. Cancellation is cooperative,
so external blocking work may still need to finish.

After work stops, the host can install the next inputs and start another operation.
The normal lifecycle stays synchronous even when some computation uses workers.
