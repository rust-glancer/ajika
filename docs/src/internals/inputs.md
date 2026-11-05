# Input snapshots and revisions

Queries read a captured input view. The host builds another view when files,
configuration, or editor buffers change, then installs it between operations.

The useful distinction is between **replacing an observation** and **capturing
an observation for the first time**. Replacement belongs to the host. Lazy
capture may happen while queries run.

## Sharing a view

[`Inputs`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/input/mod.rs#L92) contains typed field tables. A field table maps keys to
captured values and their stamps, durability, and lifetime. For a file declaration,
text and language are separate tables because they are separately tracked fields.

Cloning `Inputs` shares those tables. If neither view has read `notes.txt` yet,
the first read through either can capture its text for both. Once captured, that
entry supplies the same observation to later readers.

Replacing that text must leave the old view usable. `Inputs::edit` creates a
private table map for the candidate, then copies the text field's table on its
first write. Subsequent writes in the batch reuse that private table; other fields
remain shared. Table copies clone each value. For `#[input(arc)] field: T`, the
generated `Input::Value` is `Arc<T>`, so copying the table or reading the field
clones the pointer while keeping the payload in place.

The edit returns a candidate only after its closure succeeds.
`Database::update_inputs` installs that result; a failed batch leaves the
installed view alone.

## Sparse editor views

For a large source inventory, copying the text table to edit one file can still
be costly. An overlay adds small local tables over a base `Inputs` instead.
A draft can override just `notes.txt`; reads of other files fall through to the
saved base.

If the host forgets `notes.txt` in the overlay to reacquire it, deleting the local
entry would expose the base's old text again. A forgotten entry therefore masks
that key in the base until the overlay acquires a replacement. The table keeps
this distinction between an unseen key and a forgotten observation.

First acquisition also needs a destination. If the overlay has no local table
for a field, it captures unseen keys through the base, so the host sharing that
base sees discovered files too. Once the field has a local table, new captures
for it stay local. [`Inputs::read`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/input/mod.rs#L221) chooses where to acquire; `lookup` in the same
module checks overrides and forgotten entries before falling through.

Building each successive draft over the saved base keeps lookup from walking a
growing history of edits. Overlay composition retains that chain of views rather
than flattening it.

## First acquisition

When `notes.txt` has no captured text, Ajika calls the field's initializer without
holding the table lock. It computes the stamp and policy from the result, then
installs the observation unless another reader already installed one. Two workers
can therefore acquire the file concurrently, but both receive the winning observation.

This keeps file I/O out of critical sections while giving the view one answer
per field/key. Different files can still be acquired at different times. If the
application needs them to describe one filesystem revision, its acquisition layer
must verify that condition or reject the operation.

To validate a query that read this text, Ajika only needs the captured stamp and
policies. `Inputs::record`, next to `read`, returns that metadata without cloning
the input value. It triggers acquisition only if the field/key is still unseen.

## Publishing a new revision

[`replace_inputs`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/mod.rs#L210) compares the old and new view's stamps and
policies. Shared tables can be skipped; sparse views can compare their differences
over a shared base.

If all stamps and policies match, the revision stays the same. Otherwise the owner
advances its process-local revision and records which durability classes may have
been affected. Both the old and new policies matter when a field changes class.

For example, a query depending only on high-durability configuration can remain
verified after low-durability source edits. A query that reads both configuration
and text inherits the lower class. If configuration changes, the high-durability
shortcut is invalidated too.

The new revision tells the evaluator that an earlier validity check may no longer
be enough. Dependency validation decides which functions actually need to run;
an equal child answer can still spare its parents from execution.

## Policy changes also flow through equal answers

Imagine saving an editor buffer without changing its text. The stamp stays equal,
but its lifetime changes from session to persistent. A parent can still reuse the
same answer while refreshing its inherited policy.

Validation therefore gathers the current durability and lifetime even when all
observed stamps and versions match. Updating that policy may require a checkpoint
without executing the function. Treating “equal answer” as “no metadata changed”
would leave saved work stranded in the session store.
