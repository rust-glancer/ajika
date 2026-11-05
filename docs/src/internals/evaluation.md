# Deciding whether to execute

A stored answer can be correct without being loaded, and a loaded old answer can
be wrong for new inputs. The evaluator treats validity and residency separately.

A direct call to `word_count` needs the count itself. To validate another query
that used the count, Ajika may only need its version. Both enter [`Db::ensure`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/evaluate.rs#L214);
the internal `want_value` argument tells the evaluator whether to acquire the
primary value as well as establish its current version.

## Start with the cheapest available proof

The output metadata is grouped in a `RecordHeader`: slots, versions, stored
payload lengths, and inherited durability and lifetime. It leaves out the
dependency list and the values themselves. Once a call is known to be valid,
this header is enough to select and load one of its outputs.

After finding the operation's memo for the query/key, the evaluator checks whether
it already has what the caller needs at the current revision. This is the shared
working-set fast path.

Otherwise it acquires a claim for this call and checks again: another worker may
have finished while this one waited. It then obtains the saved record or header
and the backend's **verified revision**: the input revision at which that record
was last proved valid in this process.

There are three ways to establish validity without executing:

1. The call was verified at this input revision.
2. Its inherited durability proves that no relevant class of input changed since
   its previous verification.
3. Rechecking its saved dependencies finds the same observations.

The first two only need the output header. The third needs the full dependency
record. After reopening, the verified revision is zero, so a saved answer cannot
take a process-local shortcut before checking fresh inputs.

[`RecordHeader`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/record.rs#L146) comes first in the record's encoding so the runtime can
decode it without allocating the dependency vector. Storage may still fetch the
whole encoded row; the saving here is in constructing Rust objects from those bytes.

## Red-green validation

“Red-green” describes the distinction between an answer that may have changed
and one proved unchanged. Ajika expresses that proof through output versions.

For each saved dependency, [`validate`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/evaluate.rs#L411) does the following:

- For an input, acquire its current stamp if needed and compare it with the saved
  stamp.
- For a child output, replay the child to establish its current versions, then
  compare the selected slot's version with the saved version.

It stops at the first mismatch. A missing registration or a removed output also
prevents the old proof from succeeding. Errors from acquisition or a registered
query fail the operation; they are not converted into evidence that the data changed.

Consider the extra-space edit:

```text
File::text:  stamp S1 → S2
words:       executes, ["hello", "world"] == previous value → keeps W1
word_count:  observed W1, still sees W1 → reuses C1
```

The parent discovers that it is still valid by making its child current. It does
not need to load the child's vector merely to compare W1 with W1. The child may
have loaded its own old vector to establish equality during recomputation.

There is no eager walk marking every downstream node invalid when an input changes.
Work starts from requested roots and follows the evidence needed for those roots.

## Obtain a value only when needed

Once validity is established, a validation-only request can return the memo's
header. A normal call still needs an `Arc<Q::Value>`.

For a persistent result, the evaluator loads the selected payload through its
codec and runs the query's optional structural `validate` hook. For a transient
result, no retained primary payload exists, so the function runs to reconstruct it.

For example, suppose `words` were declared transient. With unchanged text, a
later request for its vector still has to run the function. The dependency check
has already established that it will read the same observations, so determinism
lets the evaluator preserve W1 without an old vector to compare against. If the
text changed, that proof would no longer apply; the evaluator would need the
comparison rules below.

## Execute and compare

[`compute`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/evaluate.rs#L499) runs the provider in a fresh frame. After success, the
evaluator considers the primary result and every named output separately:

| Evidence | Version decision |
| --- | --- |
| No previous slot | Create a fresh version. |
| Valid reconstruction | Preserve by proof. |
| Comparable previous payload is equal | Preserve by comparison. |
| Comparable previous payload differs | Assign a fresh version. |
| Previous payload cannot be compared | Assign a fresh version conservatively. |

A fresh version therefore means that equality was not established. It does not
always mean the values were observed to differ.

The memo is marked **dirty** when its completed record or payloads need
checkpointing. New reads or policies can require this even when every output
version is preserved. Conversely, reconstructing a valid transient value with
identical metadata doesn't necessarily cause a write.

Only after all of this succeeds does the evaluator install the record, values,
and verified revision together in the memo. Waiting callers can then use them.
Saving those results beyond the operation is a separate checkpoint step.
