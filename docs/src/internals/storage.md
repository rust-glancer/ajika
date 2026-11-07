# Storage and reclamation

The runtime understands queries, versions, and dependencies. `ajika_storage`
understands opaque record bytes and value bytes. It provides a committed view of
those bytes without retaining all decoded query state.

The division lets Ajika use a database for atomic indexing while putting large
payloads in a layout suited to replacing whole query outputs.

## What lives where

A disk cache has three main parts:

```text
owner.lock    one owner for this cache directory
index.redb    records, interner state, value locations, pack accounting
packs/       larger payloads and their headers
```

A **pack** is a file holding many encoded values. Each value occupies a **storage
frame**: its payload plus a header containing its ID, lengths, compression format,
and integrity information. These are file entries; the query `Frame` described
earlier is the runtime's temporary log of reads and outputs.

The redb index supplies atomic publication and lookup. Values up to 1 KiB stay
inline; larger values get index locations pointing into packs. Storage can read
one frame without decoding its neighbors. It checks the frame and decompresses
the payload, then the runtime's serialization codec reconstructs an owned Rust
value from those bytes. The temporary byte buffer is released before returning
the query result.

Packs target 64 MiB, allowing a larger individual frame to exceed that size.
Replacing a value changes its index location rather than overwriting its old
frame. The old space becomes reclaimable after the replacement commits.

The [storage database](https://github.com/rust-glancer/ajika/blob/main/crates/ajika-storage/src/database/mod.rs#L73) owns the index and pack readers.
[`FrameHeader`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika-storage/src/format/mod.rs#L65) defines the information needed to read or relocate one stored value.

## Publish files before references to them

A committed index location must point to a fully written payload. A write starts
from the previous committed index, appends prepared payloads to private packs,
and builds new index entries. At commit, it finishes and synchronizes the packs
before publishing their locations in redb.

Dropping an uncommitted write preserves the previous checkpoint. If commit itself
fails with an uncertain outcome, the owner stops serving the cache and leaves
cleanup to reopening, which can inspect the committed catalog.

Read views pin their backing files. The storage API requires their scopes to end
before writing; returned byte buffers are owned and do not pin a read view.
Ajika uses short reads while evaluating, then writes at the operation boundary.

The [transaction implementation](https://github.com/rust-glancer/ajika/blob/main/crates/ajika-storage/src/transaction/mod.rs#L185) contains the commit sequence that makes pack
locations visible. On Unix, it synchronizes directory entries as well as files.
The Windows path does not provide equivalent directory-name durability after
power loss, so that case may require rebuilding the cache.

## Compression belongs to individual values

The runtime first serializes a Rust value into bytes. Compression reduces those
bytes for storage; reading reverses compression before Rust decoding. The
evaluator decides equality using Rust values; these transformations determine
how those values are stored.

LZ4 is the default. The alternatives are raw bytes and level-three Zstandard.
Small inline values stay raw, and larger values also stay raw if compression saves
less than one eighth. Each frame identifies its compression format, so changing
policy affects new writes without invalidating old frames.

Maintenance can move already compressed frames without reconstructing their Rust
values or comparing them again.

## Logical eviction and physical reclamation

The runtime first decides which query records to forget, based on last use,
entry count, and logical byte budgets. It removes each selected record, its
outputs, and its process-local validity proof together.

Reads update last-use ages in memory. Collection batches those age updates into
storage writes, avoiding a metadata write for every read-only operation.

This is not reachability collection. A retained parent may still name an evicted
child; its full replay address allows reconstruction, at the cost of lost reuse.
Removing source entities does not immediately discover every now-unreachable
query. Budgets bound retained work instead.

Removing an index location does not necessarily free a whole pack: other live
values may share the file. Routine [maintenance](https://github.com/rust-glancer/ajika/blob/main/crates/ajika-storage/src/maintenance/mod.rs#L35) copies live
frames out of selected sparse packs, commits their new locations and scan
position, then removes unreferenced files. A restart resumes from committed
progress. Small packs are consolidated in groups to avoid leaving a file for
every small checkpoint.

Normal passes have byte and frame-count budgets and finish an individual frame
once started. That bounds work, not exact latency. `compact()` finishes reclamation
and shrinks the index without the normal pass budget.

Physical headroom is separate from query retention. At storage capacity, a
`Capacity` error refuses new payloads while allowing reads, removals, and
maintenance. Reclamation cannot make a live dataset smaller; the host may need
to evict work or change its storage allocation.

## What remains in memory

Between operations, the disk backend keeps compact usage entries: call ID,
last-use time, logical size, and verified revision. It does not retain every
query's encoded key, dependency vector, output metadata, and mutex.

On reopening, [`Backend::open`](https://github.com/rust-glancer/ajika/blob/main/crates/ajika/src/runtime/backend/mod.rs#L90) rebuilds that inventory from
record metadata without decoding query payloads. Verified revisions begin at
zero. Full records and headers are acquired into operation memos when needed.

The storage layer also keeps a small cache of open pack files to avoid reopening
them for every read, and redb has its own page cache. These remain useful between
operations, even though the runtime has released its decoded query values.

Inputs, interner state, bounded session values, and answers kept by the host add
to that memory use. Releasing query working state does not guarantee an allocator
immediately returns all freed pages to the OS.

Interned keys outlive many operations. If temporary discovery data was allocated
beside them, releasing that data can leave allocator pages occupied by a few
long-lived keys. Collection relocates the keys into fresh allocations after
releasing query working values, allowing those old pages to become unused.

Identity assignments have a wider lifetime than query retention: IDs cannot be
recycled while saved keys or live callers may contain them. Ordinary collection
keeps them. A reopen-time size safeguard can reset interner state and the query
cache together before handing either out.

## Rebuildable, with explicit boundaries

Epoch mismatches and unreadable backing can cause a cache reset while the owner
lock remains held. This does not mean every malformed runtime value is silently
repaired: decode or validation errors can still fail an operation.

The format is for application-owned, throwaway cache data, not hostile input or
authoritative user documents. Recovery aims to retain a coherent checkpoint or
rebuild, rather than salvage every possible cached answer.

For lower-level API examples and exact maintenance thresholds, see the
[storage crate guide](https://github.com/rust-glancer/ajika/blob/main/crates/ajika-storage/README.md).
