# Publishing independent outputs

The [tutorial's output example](../using/outputs.md) extends the text analyzer to
produce words and their byte locations in one `analyze(file)` call. Its return
value is a set of output handles naming `words` and `locations`; the handles do
not contain those payloads.

This separates the cost of producing data from the decision to load or depend on
each part.

## Emission remains local until completion

[`Db::output`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/evaluate.rs#L90) derives a slot address from the running producer
and the supplied name or name/key pair. It puts the typed value in that producer's
frame and returns an `Output<T>` handle.

The `#[ajika::output]` constructor is a convenience wrapper around these calls.
It does not register another kind of query or write a storage row itself.

The function can pass handles back in its return value, but must finish before
other queries can read its new outputs. Reading its own pending handle would
require that unfinished producer to complete, so it is rejected.

If the function fails, its frame and pending outputs are discarded. If it
succeeds, the evaluator compares all outputs and publishes their versions,
values, and replacement dependency record together. Waiting callers can then
read the completed set.

## One record, several versions

For the initial text, the record might look like:

```text
analyze("notes.txt")
  reads: File::text("notes.txt") at S1
  outputs:
    <return>    version H1   (the set of output handles)
    words       version W1
    locations   version L1
```

After inserting a space, the new input stamp is S2. The handles still contain
the same addresses, words are equal, and locations differ:

```text
reads:   S1 → S2
versions: H1, W1, L2
```

A consumer of `words` can stay valid. A consumer of `locations` must recheck its
answer. Both depend on versions in the same producer's output metadata.

All these outputs share the execution's dependency list, durability, and lifetime.
Independent versions do not imply independent execution or retention policies.

## Reading a retained handle

A retained `words` handle already identifies `analyze(file)` and its `words` slot.
Reading it does not require loading the producer's returned set of handles just
to find that same slot again.

[`output_value`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/evaluate.rs#L120) uses replay to establish the producer's current versions
without requesting its primary return value, just as dependency validation does.
It then selects the slot and loads that payload if needed.

The read records the chosen slot's version on the calling query's frame. The
operation and frame can then share that acquired `Arc` with repeated callers.

If the producer needs to execute, it still executes in full. If the selected slot
is gone afterward, a direct handle read fails. In dependency validation, that same
absence instead requires the parent to execute again, so it can discover which
outputs the new inputs call for.

## The returned set records membership

For dynamically keyed outputs, the returned handles also describe which slots
exist. A code analyzer might emit one output per function body. If an edit deletes
function B, a producer that previously returned body handles A and B should now
return only A. Consumers observe that membership change before requesting individual bodies.

Keeping B's old handle does not establish that B still exists. Handles name slots
under the active input view; they are not historical snapshots.

Omitting a formerly emitted slot removes it from the new output metadata.
Checkpointing then removes its durable location or masks the durable slot with the
session producer's new metadata. Old file bytes may remain until storage maintenance,
but query readers can no longer observe them through that slot.

When adding a new output-producing abstraction, preserve these two boundaries:
the producer owns publication, and every consumer records its own selected read.
An accessor that hides either boundary can accidentally make the whole returned set
a dependency or lose a dependency altogether.
