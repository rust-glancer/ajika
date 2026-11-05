# Ajika

Ajika runs synchronous queries and remembers which inputs they read. A query call
returns an `Arc<T>`: it may reuse a resident value, load a stored value, or compute
one. Callers use the same function in every case.

The database remembers validity separately from decoded values. During an operation,
repeated calls share their results. After a disk-backed operation finishes, Ajika
checkpoints new results and releases its decoded working set. The next operation
can load an already verified result without walking its dependencies again.

## Start with a query

First declare the external values a query can read. The declaration below describes
one source file per path. Ajika stores its text; the application supplies changes.

```rust
use std::time::Duration;
use ajika::{Database, Inputs, Result, Retention};

#[ajika::input(key = String)]
struct File {
    #[input(arc)]
    text: String,
}

type Db<'a> = ajika::Db<'a>;

#[ajika::query]
fn words(db: &Db, path: String) -> Result<Vec<String>> {
    let text = File::at(path).text(db)?;
    Ok(text.split_whitespace().map(str::to_owned).collect())
}

#[ajika::query]
fn word_count(db: &Db, path: String) -> Result<usize> {
    Ok(words(db, path)?.len())
}

# fn main() -> Result<()> {
let limits = Retention {
    max_entries: 10_000,
    max_bytes: 256 * 1024 * 1024,
    max_idle: Duration::from_secs(24 * 60 * 60),
};
// The unit value is for host services; this example needs none.
let mut database = Database::memory(Inputs::default(), (), limits);
let file = File::at("lib.rs".into());
database.update_inputs(|update| file.set_text(update, "hello world".to_owned()))?;
assert_eq!(*database.run(|db| word_count(db, "lib.rs".into()))?, 2);

database.update_inputs(|update| file.set_text(update, "hello  world".to_owned()))?;
assert_eq!(*database.run(|db| word_count(db, "lib.rs".into()))?, 2);
assert_eq!(database.statistics().executed, 1); // words ran; word_count did not.
# Ok(())
# }
```

`File::at(path)` only addresses an input. Its generated `text(db)` getter reads the
value and makes the calling query depend on it. A declaration can have several
fields; reading one does not depend on the others. `#[input(arc)]` shares the value:
setters and initializers accept owned values or existing Arcs, and getters return
`Result<Arc<T>>`. Other fields are copied or cloned as declared. See the book's
[input guide](https://rust-glancer.github.io/ajika/using/inputs.html) for field policies.

Declaration fields describe inputs, not ordinary Rust struct storage. All generated
getters, setters and field selectors are public on the handle; the declaration's
visibility controls access to that handle. Field visibility does not restrict the
generated API, so fields need no `pub`.

Setters write into a private batch. A successful `update_inputs` publishes all its
changes before another operation can run. An error leaves the installed inputs
unchanged. Assigning an equal value preserves existing validity proofs.

Adding another derived query needs a function, an owned key, and an immutable result.
Declare `Result<T>` and return an owned value from the body. The query macro wraps
successful results in `Arc<T>`; callers receive `Result<Arc<T>>`. This also applies
to transient queries and associated queries.

When a body already has an `Arc<T>`, use `forward` to reuse that allocation. For
example, a query can return a captured input without cloning its text:

```rust
# use std::sync::Arc;
# use ajika::{Db, Result};
# #[ajika::input(key = String)]
# struct File { #[input(arc)] text: String }
#[ajika::query(forward)]
fn source_text(db: &Db<'_>, path: String) -> Result<Arc<String>> {
    File::at(path).text(db)
}
```

The same option can forward another query's result or a named output's value.
Combine it with `transient` when the payload should only live in the operation.
Handwritten `Query::execute` implementations return `Result<Arc<T>>` directly.

To retain results across restarts, construct the owner with
`Database::open(path, schema_epoch, inputs, services, limits)` instead. The memory backend
retains typed persistent values without encoding them. The disk path names a storage
directory: redb holds its index and larger values live in immutable pack files.
Query keys and persisted values use wincode and normally derive `SchemaRead`/`SchemaWrite`.
The limits above are examples, not universal workspace defaults.

## Queries beside their types

When a type naturally owns a computation, put its query in an inherent impl. Mark
the impl and the query functions; ordinary methods and constants keep their Rust behavior.

```rust
use ajika::{Db, Result};

struct WordIndex(Vec<String>);

#[ajika::query]
impl WordIndex {
    #[ajika::query(transient)]
    fn from_text(_db: &Db<'_>, text: String) -> Result<Self> {
        Ok(Self(text.split_whitespace().map(str::to_owned).collect()))
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}
# fn read(db: &Db<'_>) -> Result<()> {
let index = WordIndex::from_text(db, "hello world".into())?;
assert_eq!(index.len(), 2);
# Ok(())
# }
```

Associated queries take the context first and owned keys afterward, with no `self`
receiver. `Self` also works in key types and calls to associated helpers. The impl
must be concrete and inherent; generic and trait impls are not supported.

Default names include the owner, such as `module::WordIndex::from_text`, and normally
need no override. `name` is available for an explicit identity. `transient` and `validate`
work as for free queries, including `validate = Self::check`. A renamed dependency
can be supplied on the enclosing impl with `#[runtime::query(crate = runtime)]`.
Replay registration is automatic even when the associated function has not been called.

## An operation owns the working set

`Database::run` is the host boundary: indexing a batch, rebuilding after a save, or
answering an LSP request. Queries receive only `&Db`. The owner cannot replace inputs
while that view or its scoped workers are still running.

A successful operation checkpoints complete new records before returning. A failed
operation discards its uncheckpointed work, including otherwise successful children.
A panic still propagates, after releasing claims and decoded values. There is no
query-level commit or rollback API.

An `Arc` returned from `run` remains usable. Keeping it also keeps its memory. It is
an immutable value, not a view that follows future edits. Normally the host returns
an LSP response or compact IDs; later work calls the query again with its key.

Each operation shares query calls, decoded values and metadata through working memos.
Those memos leave with the operation. The disk backend keeps only a verified revision
beside each record's retention information. Calling the query again loads its output
header, which contains versions and storage lengths, without replaying dependencies.
The dependency list is decoded only when validation or execution needs it. Durability
can still skip unaffected queries after edits. These revision proofs are process-local;
restarting requires validation against the new captured inputs.

The memory backend keeps full records because it is their only storage. Session results
also keep their records in memory: unsaved editor work must survive between requests
without reaching disk.

Application-owned Arcs, the allocator, and redb's page cache still affect idle RSS.
Scope boundaries can be shortened if a whole indexing operation retains too much data.

## Inputs and invalidation

Ajika fingerprints each field when it is supplied or first acquired. By default,
`#[ajika::input]` hashes its encoded value. A field can use `#[input(stamp = function)]`
to identify an opaque value, such as a source entry that already knows its content
hash. That function returns `Result<Stamp>` and must cover everything query code can
observe. Input payloads themselves are not written to query storage.

Durability describes which changes can leave a query verified. Fields default to
`Low`; `#[input(durability = High)]` is useful for configuration. A function of the
value can choose the class dynamically. Ajika compares both old and new policies
when publishing changes, so the host does not supply an invalidation hint.

The database stores one active input snapshot. `database.inputs().clone()` retains
that snapshot cheaply. Use `Inputs::edit` to prepare a candidate and `replace_inputs`
to install it; restoring the accepted snapshot invalidates affected proofs normally,
without rewinding query storage. On restart, the host supplies fresh inputs and old
query results validate against their fingerprints. A new process never assumes that
its saved query metadata proves the filesystem is unchanged.

For editor buffers, `saved.overlay()` shares saved inputs and stores only overrides.
Build each editor view from the saved snapshot rather than chaining views on every
keystroke. `with_base` replaces the inherited snapshot while retaining local fields;
for example, configuration can stay fixed while the host switches source views.

A stale query checks its previous dependencies in read order. At the first changed
observation it reruns and records a new dependency list. Equal results preserve
their value versions, allowing consumers to remain valid. Comparison uses `PartialEq`,
including decoding an old value when needed; it does not assume canonical encoding.
Equal results still replace changed dependencies.

Input stamps must describe the returned value, including relevant absence. All
semantic filesystem/configuration reads must pass through an input or query. A
cache hit does not rerun provider side effects. If captured text is evicted, reloading
it must verify the same stamp or reject the operation; the filesystem is not a snapshot.

Persistent keys must survive reopening and input changes. Transient arena slots need
an owning identity. Do not put the entire source revision in every key: Ajika then
cannot find the previous logical result to compare it.

The application owns saved-project acceptance and response freshness. Ajika has one
active input revision, not independently queryable historical project snapshots.
A completed checkpoint can survive cancellation without becoming an accepted project.

## Acquiring inputs on demand

An initializer can acquire a field when its key is first requested, including during
validation of a persisted dependency. The application chooses how acquisition works:

```rust
# use ajika::Result;
#[ajika::input(key = String)]
struct File {
    #[input(arc, initialize = File::load)]
    text: String,
}

impl File {
    fn load(path: &String) -> Result<String> {
        Ok(std::fs::read_to_string(path)?)
    }
}
```

Ajika retains the first observation. Parallel acquisitions may race, but every reader
receives the winning stored value. Initializers run without a global input lock and
cannot call queries. Later filesystem events must replace or forget the observation
through an input update; repeatedly reading it does not poll the filesystem.
`update.forget(File::TEXT, path)` forgets one key, and `update.forget_all(File::TEXT)`
forgets every captured value of that field. The next read can initialize it again.
To represent a missing file, supply an absence value in an appropriate input instead.

This example retains text. An application needing low idle memory can instead share
a source entry with a fixed descriptor and an optional text buffer. Its custom
stamp uses the descriptor. Eviction changes only residency; reloading must reproduce
the captured bytes or report a stale source. Unsaved bytes need resident backing.
Ajika owns the entry, while source acquisition and eviction policy remain application code.

Host maintenance can inspect fields through their generated selectors:
`inputs.entries(File::TEXT)` enumerates captured values without loading missing ones.
`get` can initialize a value, and `peek` only inspects one already captured. These are
host-side operations; query code uses `file.text(db)` to record its dependency.

Host services such as progress reporting and cancellation are separate from inputs.
Pass them to the database constructor and access them through `db.services()`. They
are untracked: configuration that affects an answer belongs in an input field.

## Values that should not be serialized

Use `#[ajika::query(transient)]` for an owned parse tree or another intermediate that
should live only in the working scope. It needs neither serialization nor equality.
Its dependency record can still be persisted, so a stored parent can validate through
it without rebuilding the intermediate.

Handwritten queries select this policy with `Storage::transient()`. It applies to the
return value; named outputs keep their ordinary persistence rules. This is separate
from `Database::memory()`, which selects a backend that can retain values between operations.

If its value is needed after release, the provider runs again. Unchanged dependencies
preserve its known version. Changed dependencies conservatively produce a new version
when no old value is available to compare; persisted descendants can still stop
propagation by comparing their own results.

An unsaved buffer needs a different rule: its results should survive requests, but
must not replace saved work on disk. Use `#[input(lifetime = Session)]`, or a policy
function for values that can be either saved or edited. Ordinary persistent queries then retain their changed outputs
as typed values in a bounded session store. Equal outputs can still use their saved
payloads; retaining an edited declaration does not retain every declaration beside it.

Saved inputs use the default `Lifetime::Persistent`. After a save, install the new
inputs with `replace_inputs`: validation can promote equal session results to disk
without recomputing them. To discard editor work, restore saved inputs and call
`clear_session`. The saved baseline remains available throughout editing.

`Lifetime::Operation` is for observations that cannot be reused by a later request.
Their derived results and memo keys leave with the operation. A query inherits the
shortest lifetime among its observations. Input values themselves are never serialized.
Lifetime controls derived work, not acquisition: the host still replaces or forgets an
input to acquire a new observation. Updating an input's lifetime invalidates affected
proofs even when its bytes are unchanged.

`set_session_retention` bounds session producers, estimated encoded bytes, and idle age.
Collection runs after operations and when the host calls `collect`; there is no background
timer. Evicted results recompute on demand. The byte estimate avoids serializing a second
copy, but is not a hard limit on the Rust allocator's resident memory.

## One producer can have several outputs

Keep naturally fused computations together while allowing readers to load and depend
on separate results. Declare their payload types with `#[ajika::output]`; the macro
turns each field into an `Output<T>` handle and supplies an emitting constructor.
Here `Build`, `DefMap`, and `Items` stand for application types:

```rust,ignore
#[ajika::output]
struct Declarations {
    definitions: DefMap,
    items: Items,
}

#[ajika::query]
fn declarations(db: &Db, package: PackageKey) -> Result<Declarations> {
    let built = Build::run(db, package)?;
    Declarations::new(db, built.definitions, built.items)
}

fn definitions(db: &Db, package: PackageKey) -> Result<Arc<DefMap>> {
    declarations(db, package)?.definitions.value(db)
}
```

`new` accepts owned values or existing Arcs, in field order. The generated type
derives Debug, Clone, PartialEq, Eq and wincode serialization for the handles;
additional derives apply to that type. Payloads need the ordinary `Db::output`
bounds, including PartialEq, but do not need Debug, Clone or Eq. The type keeps its
declared visibility; constructor and fields are public regardless of field visibility.
Use `crate = path` when importing Ajika under another name.

Each handle identifies its producer/key and a named slot. The field name supplies
that slot; `#[output(name = "definitions")]` preserves it when renaming a Rust field.
Names must be distinct within a declaration. The declared type does not namespace them:
two sets of output handles emitted by the same query must still use different slots.
Dynamic families continue to use `db.output(("body", file_key), value)` directly,
without a declaration.
Emission order does not define identity.

Changing one output preserves equal sibling versions. The returned set can remain
equal because its output handles still name the same slots. Optional emission is explicit:

| Declaration | Generated field | Constructor argument |
| --- | --- | --- |
| `field: T` | `Output<T>` | `impl Into<Arc<T>>` |
| `#[output(optional)] field: T` | `Option<Output<T>>` | `Option<Arc<T>>` |
| `field: Option<T>` | `Output<Option<T>>` | `impl Into<Arc<Option<T>>>` |

For optional emission, pass `Some(Arc::new(value))` or plain `None`. `None` emits no
slot. Changing its presence also changes the returned set, so consumers calling the
producer may rerun even when another output stays equal. A required `Option<T>` payload keeps
its slot present and changes only that output's value.

The producer publishes its result, dependencies, and output set together. Removed
slots invalidate old consumers during validation; those consumers can request the
new output handles. Directly loading a removed slot is an error. A producer cannot
read its own unpublished output: use local builder values until computation finishes.

## Interning objects

Interning maps immutable fields to compact IDs. A file path can keep the same ID
after its text changes or a query is collected:

```rust
# fn main() -> ajika::Result<()> {
#[ajika::interned(namespace = "files")]
pub struct FileId {
    path: String,
}

// In an application, obtain this shared handle from database.interner().
let interner = ajika::Interner::default();
let file = FileId::new(&interner, "src/lib.rs".to_owned())?;
assert_eq!(file, FileId::new(&interner, "src/lib.rs".to_owned())?);
assert_eq!(file.path(&interner)?, "src/lib.rs");
# Ok(())
# }
```

The generated `FileId` is a compact, `Copy`, serializable handle. All declared fields
jointly form its encoded key; interning equal keys returns the same ID, including
across concurrent callers. IDs also support equality, ordering, hashing, and debug
formatting. Additional derives apply to the ID itself.

Each declaration has its own namespace, defaulting to its module-qualified name.
An explicit `namespace` gives it a name independent of its Rust location. Each
namespace has one numbering space per database. If identity depends on context,
include that context in the declaration's fields: for example, a module may be
identified by both its crate and its path. Ajika encodes these fields without
interpreting them or accessing the filesystem.

Each getter decodes the complete key and returns the requested field as an owned
value. It neither keeps a decoded copy resident nor records a query dependency:
the fields never change. Generated methods are public on the handle, as with input declarations;
field visibility does not restrict them. Use the same interner when
reading an ID. Unknown IDs and malformed keys return errors. `as_raw` / `from_raw`
are available for numeric boundaries such as token span formats; restoring a raw
number does not allocate an identity or check its provenance.

Assignments stay in memory until a durable query checkpoint saves them in the same
transaction as its results. Session-only work adds no disk writes. Cancellation and
`invalidate` never recycle an issued ID in a live database. The interner belongs to
that database: do not attach an old handle to a newly opened database.

Individual assignments are not collected, because other results may still refer to
them. At reopen, an interner above 32 MiB of stored names is reset together with its
query results, before any IDs are exposed. This is a coarse bound on historical names;
ordinary query retention does not determine identity lifetime.

## Parallel work and cancellation

Ordinary query calls share one computation per key. That coordination also covers
validation and decoding. Use `db.parallel_map(&items, |db, item| query(db, item))`
for nested query-aware batches; results and dependency groups retain input order.
`Database::set_parallelism` controls its worker budget.

The helper uses bounded scoped native threads and runs nested work inline when no
worker permit is available. It is intended for coarse batches. Ordinary nested
query calls do not create threads. Arbitrary nested Rayon work stealing is not a
supported substitute: it can suspend an owner underneath another task waiting for
that owner's query. External scoped native threads can use the same `Db` normally.

Obtain `database.cancellation()` before starting an operation and signal it from the
coordinator. Ajika checks cancellation at query/input boundaries and wakes waiters.
Long provider loops should call `db.check_cancelled()`. Blocking external tools still
need application-owned deadlines. Tokens belong to one operation; old tokens cannot
cancel subsequent work.

Cycles fail the operation with `QueryCycle`; there is no fixed-point recovery.
Shared failures preserve application context, such as `.context("building declarations")`.
Find the structured cycle among the error's causes:

```rust
# fn inspect(error: &anyhow::Error) {
let cycle = error
    .chain()
    .find_map(|cause| cause.downcast_ref::<ajika::QueryCycle>());
# }
```

Its `calls` chain repeats the first call at the end. Each `QueryCall` contains its
query name and full address hash. The displayed cycle abbreviates hashes:

```text
query cycle:
  target#a12f3456
    → associated#b8367890
    → target#a12f3456
```

This is a witness from active calls, including calls on other workers. It is available
even with tracing disabled. Expected language errors belong in ordinary query data.
Runtime errors poison the operation even if a caller catches an individual returned
error, preventing incomplete work from being committed.

## Understanding recomputation

The `ajika::diagnostics` tracing target explains why an answer runs or stays reusable.
Use `ajika::diagnostics=debug` for executions and failed validations, or
`ajika::diagnostics=trace` to include reuse and individual output decisions.

| Event | Level | What it reports |
| --- | --- | --- |
| `execute` | debug | `no_record`, `dependency_invalid`, or `value_unavailable` (valid work whose value needs reconstruction). |
| `invalidated` | debug | The first failed dependency actually examined: `input_stamp_changed`, `dependency_version_changed`, `output_removed`, or `registration_unavailable`. |
| `reuse` | trace | `resident_value`, `validity_proof`, `durability`, or `dependencies_valid`. Validation can reuse a version without loading its value. |
| `output` | trace | The result of evaluating each return value or named output, with its equality evidence and version action. |

An output's `result` is `added`, `equal`, `unequal`, `comparison_unavailable`, or
`removed`. Equal results carry `evidence=comparison` or `evidence=validity_proof` and
`version=preserved`. Added outputs have `version=created`; unequal or incomparable
outputs have `version=advanced`. Removed outputs have `version=removed`. Advancing
a version means equality was not established; only `result=unequal` confirms a value
difference. For example, reconstructing valid transient work preserves its version
by proof, while reconstructing it after an edit may have no old payload to compare.

Every event includes `op`, a process-local operation ID shared across workers, and
the operation's input `revision`. Operation IDs are distinct across database owners;
revision numbers belong to their database. `query` names the registered family,
`call` abbreviates its address hash to eight hex digits, and `call_id` retains the
full hash for correlation. Invalidations also identify the dependency family and
its full `dependency_id`. No key needs `Debug`, and keys are not printed.

`output` names a fixed slot, or shows `name#hash` for a keyed slot with an opaque key
hash. `<return>` denotes the query's ordinary answer. `output_id` is the full stored
identity of that value; custom slot encodings that cannot be rendered show `<opaque>`.
Slot decoding and identity formatting happen only when diagnostics are enabled.

These events describe evaluation, not durable writes. A later operation failure can
discard evaluated results; checkpointing decides what survives. Diagnostics do not
load extra payloads, compare extra values, or continue validation past its first
failed dependency.

## Storage lifecycle and maintenance

Successful operations checkpoint eligible results before releasing their decoded
working set. A crash before publication leaves the previous checkpoint intact.

`set_compression` chooses raw bytes, LZ4 (the default), or Zstd for future writes;
existing payloads remain readable.

Cold records are collected at operation boundaries when over budget, and periodically
for idle age. `collect` requests immediate collection. Surviving records keep their
validity proofs; collected queries are recomputed on demand.
Memory storage uses entry/age limits; encoded byte limits apply to disk storage.

Routine physical maintenance runs between operations with bounded work. `compact`
finishes reclamation and shrinks the index without that bound.

Storage reserves maintenance headroom near its 32 GiB soft physical budget. `StorageFull`
refuses new payloads while permitting reads and reclamation. `invalidate`
discards saved query records and their packs while preserving stable identities.
The schema epoch must change when application types **or query semantics** change.

`storage_statistics` reports logical payloads, dead pack bytes, and filesystem
allocation. It scans the index, so use it for diagnostics rather than each request.
Storage errors are returned to the host; a failed write can require reopening rather
than silently retrying an uncertain durable state.

The book's [checkpointing](https://rust-glancer.github.io/ajika/internals/checkpoints.html)
and [storage](https://rust-glancer.github.io/ajika/internals/storage.html) chapters
explain the implementation.
