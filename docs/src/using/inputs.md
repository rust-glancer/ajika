# Changing inputs

A query can only react to changes Ajika knows about. Reading the current directory,
an environment variable, or a file directly inside a query hides that observation
from the runtime. A reused query does not execute those reads again.

Put answer-affecting data in typed inputs. The application decides when to acquire
new data and publish it.

## Fields are tracked separately

Suppose the analyzer also needs a language setting:

```rust
# extern crate ajika;

#[ajika::input(key = String)]
struct File {
    #[input(arc)]
    text: String,
    language: String,
}
```

Reading `File::at(path).text(db)` depends on the text field. It does not depend on
`language`. A query that reads both fields depends on both.

`#[input(arc)]` shares the field's value through an `Arc`. Setters and initializers
accept an owned value or an existing `Arc`; getters return `Result<Arc<T>>`.
Stamp and policy callbacks inspect `&T`. Use it for large values such as source
text; ordinary fields return their declared type by copying or cloning it.

The declaration describes Ajika-managed fields, not ordinary fields on a Rust
struct. Its getters, setters, and field selectors are public; the declaration's
visibility controls access to the handle. There is no need to write `pub` on each field.

## Publish one coherent batch

Group related changes in one update:

```rust
# extern crate ajika;
# use ajika::{Database, Result};
# #[ajika::input(key = String)]
# struct File { #[input(arc)] text: String, language: String }
fn publish_files(database: &mut Database, changed: Vec<(String, String)>) -> Result<()> {
    database.update_inputs(|update| {
        for (path, text) in changed {
            let file = File::at(path);
            file.set_text(update, text)?;
            file.set_language(update, "plain-text".into())?;
        }
        Ok(())
    })
}
```

The batch becomes visible together. An error while preparing it leaves the old
inputs installed. Batching is also cheaper: a changed field table is copied once
for the batch, rather than once per setter in a sequence of separate updates.

The host may want to inspect a batch before making it active.
`database.inputs().edit(...)` prepares a candidate `Inputs` view for those checks
while the database continues to use its installed inputs. Once the host accepts
the candidate, `database.replace_inputs(candidate)` installs it.

Keeping a clone of the previous `Inputs` view lets the host restore it later.
Cloning shares captured data, while editing builds a separate set of changes.

Publishing changed fingerprints or policies advances the database's input
**revision**. Publishing equivalent observations does not. Starting another
operation or saving query results does not itself advance the revision.
Restoring an older snapshot is another input replacement, never a rewind of the
database's validation history. See [`replace_inputs`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/mod.rs#L210).

## Fingerprints describe observations

By default, the input macro computes a `Stamp` from the field's serialized value.
Ajika stores that stamp beside the captured value and later compares it with the
stamp a query recorded.

Use a custom `#[input(stamp = function)]` when a field already carries a content
fingerprint or is an opaque source object. The function receives a reference to the
field and returns `Result<Stamp>`. It must identify everything query code can observe.
A modification time alone is not necessarily a content fingerprint.

Reproducible encoding helps reuse: two logically equal unordered maps can otherwise
produce different stamps. Prefer a stable representation or a stamp computed from
the semantic content. A changed stamp can cause extra work; incorrectly reusing a
stamp can return a stale answer.

## Durability is a shortcut, not permanence

Source text usually changes often; configuration may change rarely:

```rust
# extern crate ajika;
#[ajika::input(key = ())]
struct Configuration {
    #[input(durability = High)]
    case_sensitive: bool,
}
```

Fields default to `Low` durability. If only low-durability fields change, Ajika can
leave configuration-only queries verified without inspecting their dependencies.
A query that also reads source text inherits the lower class.

High durability does **not** make a field immutable, and it does not decide whether
data reaches disk. Changing a high-durability field invalidates the corresponding
shortcut automatically. [Input lifetime](sessions.md) is the separate decision
about whether derived work may survive the operation, session, or restart.

The input implementation is described in [snapshots and revisions](../internals/inputs.md).
