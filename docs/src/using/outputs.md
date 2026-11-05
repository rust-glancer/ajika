# Separating outputs

A text analyzer may need both words and their byte ranges. Those are produced by
the same scan, but they do not change together. Inserting a space preserves the
words and moves some ranges.

Returning both in one large value makes every consumer load both and depend on
their combined equality. Named outputs let one computation publish separately
readable answers.

## Produce a set of output handles

The [outputs example](https://github.com/rust-glancer/ajika/blob/main/examples/outputs/src/main.rs)
extends our analyzer with a set of output handles:

```rust
# extern crate ajika;
{{#rustdoc_include ../../../examples/outputs/src/main.rs:producer}}
```

The macro turns `words: Vec<String>` into a public `Output<Vec<String>>` field.
`FileOutputs::new` accepts the actual values, emits them, and returns the handles.
Its arguments can also be existing `Arc`s.

The returned output handles contain addresses, not the words or locations. Equality
compares those addresses. When the same query/key emits the same slots again,
the returned set can stay equal even if one payload changes.

## Read the fact you need

```rust
# extern crate ajika;
{{#rustdoc_include ../../../examples/outputs/src/main.rs:consumer}}
```

This records a dependency on the set returned by `analyze`, then on the
`words` output read through its handle. It does not read or depend on the
`locations` payload.

Try the whitespace edit:

```rust
# extern crate ajika;
{{#rustdoc_include ../../../examples/outputs/src/main.rs:edit}}
```

`analyze` runs because the text changed. Its words compare equal and keep their
version; its locations differ and get a new version. The returned set also stays
equal, so `word_count` needs no execution.

The previously retained locations handle reads the **new** locations. A handle
names a slot under the active inputs. It is not an immutable snapshot of its old
payload. A retained `Arc<T>`, by contrast, still contains the old value.

## What splitting does and does not buy

Outputs provide separate loading and separate change propagation. They do not
make the producer partially executable: when `analyze` must run, its whole body
runs and produces both values. Use separate query functions when computations
themselves should be requested independently.

All outputs of a producer share its dependency list and reuse policy. A session
input read anywhere in that producer restricts the producer's outputs together.
The evaluator compares their payloads independently after execution.

While constructing outputs, keep using your local values. A producer cannot read
its own output handle before it finishes. Its return value and emitted slots become
available to waiting callers together.

## Optional and dynamically keyed outputs

```rust
# extern crate ajika;
#[ajika::output]
struct FileOutputs {
    words: Vec<String>,
    #[output(optional)]
    locations: Vec<(usize, usize)>,
}
```

The optional field becomes `Option<Output<Vec<(usize, usize)>>>`.
Its constructor argument is `Option<Arc<Vec<(usize, usize)>>>`.
Passing `None` emits no slot. By contrast, an ordinary field
`locations: Option<Vec<(usize, usize)>>` always emits one slot whose payload may
be `None`.

For a collection known only during execution, call
`db.output(("body", body_key), value_arc)` directly. Names and keys identify slots;
emission order does not. Each slot may be emitted once by that execution.
Field names provide fixed slot names, and `#[output(name = "locations")]` can
choose a name explicitly.

If a later execution omits a slot, a parent validating its old read is made
uncertain and can run a different branch. Directly reading an old handle to that
removed slot returns an error. Read a producer's current output handles when membership
can change.

The [`Output` implementation](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/output.rs#L65) and
[publication walkthrough](../internals/outputs.md) explain how addresses and versions
make these behaviors fit together.
