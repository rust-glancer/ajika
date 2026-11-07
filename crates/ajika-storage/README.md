# Ajika storage

This crate saves opaque records and values together. Ajika decides which query results are
valid and serializes their Rust types; this crate decides where the resulting bytes live.

Small records, stable identity assignments, and value locations are redb rows. Larger values
are frames in immutable pack files. Loading one value reads only its frame, checks its
integrity, and decompresses its bytes. A later replacement changes the index location instead
of overwriting the old frame.

## Saving a checkpoint

Encode a bounded batch of values before passing it to the writer. `Compression::encode` is
independent of a database, so callers can run encoding and compression concurrently. A single
writer then appends the prepared buffers sequentially.

```rust,no_run
use ajika_storage::{Compression, Database, Options};

# fn main() -> anyhow::Result<()> {
let database = Database::open(std::path::Path::new("query-storage"), "schema-1", Options::default())?;
let id = [42; 32];
let bytes = b"already serialized result".to_vec();
let length = bytes.len() as u64;
let value = Compression::Lz4.encode(bytes)?;

let mut write = database.begin_write()?;
write.write_value(&id, value)?;
write.set_metadata(&id, b"caller-owned record")?;
write.commit()?;

let bytes = database.snapshot()?.read_value(&id, length)?;
# Ok(())
# }
```

`commit` finishes buffered writes and synchronizes new packs before publishing their locations
and records in one redb transaction. Dropping a write before commit leaves the previous
checkpoint. A commit error can have an uncertain outcome: the owner then rejects further use
until `Database::reopen` resolves the published checkpoint. The directory lock
stays held, and the recovered catalog determines which new files are safe to remove.

Read views pin their backing files. Finish those views before starting a write; overlapping
read/write scopes return an error. Returned buffers are owned and impose no such restriction.
A small cache of ordinary file handles amortizes opens without retaining mapped payloads or
decoded data.

## Compression and layout

`None` stores raw bytes. `Lz4` uses independent LZ4 frames; `Zstd` uses independent level-three
Zstd frames. Values of at most 1 KiB stay raw and inline. Other values fall back to raw storage
when compression saves less than one eighth. Every frame records its own codec, so policies
can change without resetting the cache.

One pack targets 64 MiB, with large individual values allowed to exceed it. Creating files is
lazy: an entirely inline checkpoint needs none. The instance directory contains `owner.lock`,
`index.redb`, and `packs/`. Keep the lock file in place when resetting contents. An incompatible
epoch or confirmed corruption can cause a reset under that lock. If the index
is unreadable, `Database::rebuild` can publish an empty catalog with caller-supplied
identity assignments. On Unix, new files and
directory entries are synchronized before publication; Windows can require cache reconstruction
after power loss because equivalent directory-name durability is not implemented.

## Reclaiming space

Removing a value makes its old frame unused. The file can disappear immediately if it has no
live neighbors. Otherwise, call `maintain` between operations, after releasing read views.
Ajika does this automatically; hosts using this storage crate directly must schedule it themselves.

A pass copies live frames out of a selected pack, preserving compressed bytes and checksums.
It publishes the new locations and its scan position together, then removes files with no live
values. A restart resumes from the committed position. Query records, versions, and identities
do not change. The copy uses a fixed-size buffer; it never deserializes query values.

Normal passes scan at most 16 MiB or 4,096 frames, finishing an individual frame even if it exceeds
the byte budget. This bounds work, not wall-clock latency. They select packs with at least 1 MiB
and 25% dead space, or finish a previously started evacuation. Groups of eight small, similarly
sized packs are consolidated too, so individual requests do not each leave a permanent file.
Size bands avoid repeatedly copying a growing pack after every few tiny writes.

`compact` explicitly finishes reclamation and shrinks the redb index. It is unbounded and belongs
at an idle boundary. Normal maintenance deliberately leaves small holes; `compact` removes those
too. Dense or large live packs need not be merged into a single file.

`Options::max_physical_bytes` defaults to 32 GiB. It is a soft physical budget, separate from
Ajika's logical retention policy. Normal writes reserve headroom for maintenance; index growth
within a checkpoint and copying oversized values can exceed the limit. At capacity, new payloads
return an error classified as `ajika_storage::ErrorKind::Capacity`, while reads, removals, and maintenance remain available. Space reclamation
cannot help if the live working set itself exceeds the budget: callers must discard cached work
or allow more space. There is no automatic eviction or rebuild loop in this crate.

## Buffering and diagnostics

Values use complete encoded buffers. Ajika bounds each parallel batch by raw size and runs
an oversized value alone. Large-value streaming is not implemented; neither compressor
contexts nor the batch's encoded buffers are retained after encoding completes.

`Database::statistics` scans index metadata without decoding query payloads. It distinguishes
raw bytes, compressed payload bytes, inline bytes, dead space, file lengths, and allocated disk
blocks. For a closed cache, `Statistics::inspect` and the `inspect` example provide the same
read-only report without resetting or repairing anything:

```text
cargo run -p ajika_storage --example inspect -- path/to/query-storage
```

If a process exited without closing redb, the index may first need normal recovery. The
read-only inspector reports that condition instead of silently opening the original for writes.
