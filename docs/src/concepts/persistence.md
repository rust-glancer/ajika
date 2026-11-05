# Owning the cache

A persistent Ajika database is a way to reuse derived work across process
lifetimes. The application must still be able to acquire its inputs and compute
its answers if the cache disappears.

This gives the host a simpler responsibility than maintaining an authoritative
database: decide which inputs are current, which stored formats are compatible,
and how much space reusable work may occupy.

## Reopen the cache before supplying inputs

When the application starts, `Database::open` restores saved query work and
interner state. That gives Ajika previous answers to consider, but files may have
changed while the application was closed. The host supplies current inputs so
Ajika can tell which saved answers still apply.

Those inputs can be supplied up front or acquired through initializers when first
read. Ajika checks saved answers as queries are requested, so startup doesn't
require loading every input or validating every cached query.

If your inputs refer to files by interned IDs, those IDs have to match the ones
in saved query keys. The restored interner keeps that mapping, so you can use the
handle from `Database::interner()` while building the inputs. Each file will then
keep its earlier ID even if you discover the files in a different order.

Only one owner can open a storage directory at a time. Workers share the owner's
operation; they do not each open the directory as a separate database.

## Change the epoch when the meaning of saved work changes

Ajika can detect changed tracked inputs. It cannot infer that editing a query's
Rust implementation should invalidate yesterday's answer.

For example, changing a tokenizer's rules may produce different tokens for the
same source text, even though the input and result types stay the same. Reusing
old tokens would hide that change from the queries that consume them.

The application's epoch supplied to `open` marks compatible saved work.
Change it when query semantics or encoded keys and values become incompatible.
A changed epoch rebuilds the cache rather than migrating old results.

Use input changes for ordinary edits. Resetting the entire database for every
file update loses the reuse that query dependencies provide.

## Distinguish eviction, invalidation, and a full reset

Eviction forgets selected reusable results to stay within budgets. A later
request can compute them again. It is not the same as deciding that a changed
input makes an answer outdated.

`Database::invalidate` discards retained query work while preserving installed
inputs and interner state. The next operation computes against those
inputs without old query answers.

A full storage reset, including an incompatible epoch, can discard interner
state too. Keep interned IDs and saved output handles within the originating
cache's lifecycle. They should not become the only identifiers in an application's
authoritative data.

## Schedule maintenance around application work

Successful operations already perform periodic or budget-triggered collection
and bounded physical maintenance. A host can call `collect` at another idle
boundary. There is no background timer advancing collection while the application
does nothing.

Eviction can remove records while leaving their storage files partly in use.
The remaining records must be moved before those files can be deleted, so physical
disk usage may fall later than logical cache usage. Routine maintenance does this
gradually; `compact` finishes reclamation and shrinks the index. It can take longer
than routine maintenance because it completes that work in one call.

If disk usage stays high after eviction, `statistics().stored_bytes` shows how
much logical data remains, while `storage_statistics()` reports physical disk
usage. The [cost and reuse guide](../using/diagnostics.md) explains how to read those
measurements and how compression affects stored size.

If the backend reports an uncertain write failure, reopen it or successfully
invalidate it before continuing. Rebuilding is an acceptable recovery for derived
data. Application inputs and accepted user-visible state must remain available
independently of that recovery.
