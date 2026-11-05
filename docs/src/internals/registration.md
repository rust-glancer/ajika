# Calls, addresses, and registration

A normal Rust call knows which function to invoke. A dependency saved yesterday
must describe that call as data, so a new process can make it again.

For `words("notes.txt")`, that description contains two things:

- The query family's stable name, such as `example::words`.
- The encoded key, `"notes.txt"`.

Together they form an [`Address`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/record.rs#L34). Inputs use the same kind of address
for a field family and key.

## The same question can have a new answer

The storage ID is a 32-byte hash of that address. It identifies the question, not
the answer's contents. Changing the file replaces work at the same logical address
instead of creating a new historical entry for every text version.

The ordinary return value uses the call's ID. A named output derives another ID
from the producer's ID and its encoded slot. The name `"locations"` on two
different producers therefore identifies two different outputs.

Ajika keeps the full address in saved dependencies as well as using hashes for
lookup. If the child's record is collected, the parent still has the key needed
to recompute that child.

The address identifies the question; other identifiers describe what happened
when answering it:

| Identifier | Meaning |
| --- | --- |
| Call or output ID | Where this logical answer belongs. |
| Input stamp | What external data a query observed. |
| Output version | Which answer downstream consumers observed. |
| Input revision | When this database owner last published relevant changes. |

Output versions are fresh 16-byte random tokens when equality cannot be established.
They are neither content hashes nor revision counters. Random tokens avoid
accidentally repeating a forgotten version after a record has been collected.

## From a declaration to a callable provider

`Query` is the evaluator's interface to a computation. `#[ajika::query]` generates
a **provider** type implementing that trait, and leaves a calling wrapper where
the function was declared. The wrapper passes the provider and owned key to the
evaluator; the provider's `execute` method invokes the original computation.

The body declares `Result<T>`, while the wrapper returns `Result<Arc<T>>`. The
provider wraps successful values in an Arc. With `forward`, the body already
returns `Result<Arc<T>>`, and the provider passes that Arc through.

One key argument is used directly; zero or multiple key arguments become a tuple.
Associated queries use the same machinery. Their computation stays in the owning
impl so `Self` and helper calls keep normal Rust meaning.

For a saved address, the runtime has only a family name and encoded key, so it
also needs a way to find this provider and decode its arguments. The macro emits
a registration gathered by `inventory` when the program links. At database
construction, [`Registry::linked`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/registry.rs#L38) collects the query implementations for
that database's services type, alongside the input declarations.

This means replay works before the application has directly called every query
function. There is no “warm up registrations by executing queries” step.
Handwritten `Query` implementations use `#[ajika::register]` for the same purpose.

## Replay enters the normal evaluator

When validating `word_count`, Ajika checks whether `words` still has the version
the count depended on. Comparing those versions does not require the word vector
itself. [`Registry::replay`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/registry.rs#L75) resolves the saved family name, decodes the key,
and enters the same evaluator used by a typed query call.

If the child's dependencies still match, the evaluator can establish its current
version without loading that vector. Changed dependencies may require executing
`words` first. The [evaluation chapter](evaluation.md) follows both paths.

If the current program has no registration for the saved query name, Ajika cannot
check that old dependency. It reruns the parent instead, letting its function
collect fresh reads. An error from a registered provider fails the operation.

Two different implementations cannot share a family name within the same
registry. In-process `TypeId` checks detect that collision; `TypeId` is not
part of the persistent address.

Changing query code or encoded types still needs an application epoch change
when stored results are no longer compatible. Registration describes how to
call code; it does not fingerprint the implementation.

## Interning is separate from query registration

The registry connects query names to executable code. The interner instead gives
application values, such as file paths, compact IDs. An interned `FileId` can
appear inside many query keys, each of which identifies a different question
about that file.

The interner is shared for the database's lifetime and restored before queries
intern new objects. Each declaration has its own numbering space. Looking up an
ID's fields does not add a query dependency because the assignment is immutable.

See [`Interner`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/interner/mod.rs#L33) for assignment ownership and the
[user chapter](../using/interning.md) for its reset boundary. The interner must
remain consistent with every saved key that contains its IDs.
