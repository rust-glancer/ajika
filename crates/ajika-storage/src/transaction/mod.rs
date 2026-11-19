//! Read complete values, or publish a batch of replacements.
//!
//! The index is the only publication authority. New frames may already exist on disk while a
//! write is pending, but no reader can find them through the old index. Pack accounting changes
//! in the same commit as locations, so deleting the last value can retire its entire file.

use std::{
    collections::BTreeMap,
    error::Error as _,
    fs::File,
    io,
    ops::{Bound, Range},
    sync::{Arc, RwLockReadGuard, RwLockWriteGuard, atomic::Ordering},
    time::Instant,
};

use redb::{ReadableDatabase, ReadableTable};

use crate::{
    Database, EncodedValue, Error, Id, Result,
    error::ensure,
    format::{
        self, FrameHeader, IDENTITIES, Location, PACKS, PackHeader, PackId, PackInfo, RECORDS,
        SHARED, VALUES,
    },
    pack::PackWriter,
    value::INLINE_BYTES,
};

// How many open files we might want to have. This is a value that should be
// sufficient for reasonably big projects, while still providing a bound.
const MAX_CACHED_PACK_HANDLES: usize = 64;

// Reconciliation can change the available bytes, so choose the kind of limit here
// and calculate the budget after refreshing disk usage under the write lock.
enum WriteMode {
    Normal,
    Maintenance,
}

/// A checkpoint under construction. Dropping it before commit preserves the previous checkpoint.
/// Reads must finish before beginning a write on the same database.
pub struct Write<'a> {
    pub(crate) transaction: redb::WriteTransaction,
    database: &'a Database,
    pub(crate) packs: PackWriter<'a>,
    pub(crate) changed_packs: BTreeMap<PackId, PackInfo>,
    // An empty range asks the next allocation to find free IDs. None keeps the rest of
    // this checkpoint inline after two misses. The exclusive end can be one past u32::MAX.
    shared_ids: Option<Range<u64>>,
    _guard: RwLockWriteGuard<'a, ()>,
}

impl<'a> Write<'a> {
    pub(crate) fn new(database: &'a Database) -> Result<Self> {
        Self::start(database, WriteMode::Normal)
    }

    /// Maintenance is bounded by its scan/copy pass, not by the exhausted normal-write budget.
    /// It must still be able to release space after index growth crosses that soft limit.
    pub(crate) fn maintenance(database: &'a Database) -> Result<Self> {
        Self::start(database, WriteMode::Maintenance)
    }

    fn start(database: &'a Database, mode: WriteMode) -> Result<Self> {
        let guard = database
            .access
            .try_write()
            .map_err(|_| Error::access_conflict("finish storage reads before starting a write"))?;
        let index = database.index()?;
        if database.reconciliation_needed() {
            database.reconcile_files()?;
        }
        // A previous transaction may have grown the index even if it was rolled back.
        // Refresh it before calculating the budget, while this writer has exclusive access.
        database.refresh_index_usage()?;
        let remaining = match mode {
            WriteMode::Normal => database.write_budget(),
            WriteMode::Maintenance => u64::MAX,
        };
        Ok(Self {
            transaction: index.begin_write()?,
            database,
            packs: PackWriter::new(
                &database.packs,
                database.instance,
                remaining,
                &database.reconciliation_due,
            ),
            changed_packs: BTreeMap::new(),
            shared_ids: Some(0..0),
            _guard: guard,
        })
    }

    pub fn set_identity(&self, namespace: &str, id: u32, key: &[u8]) -> Result<()> {
        self.transaction
            .open_table(IDENTITIES)?
            .insert((namespace, id), key)?;
        Ok(())
    }

    /// Append prepared bytes and replace their location. This does no query serialization or
    /// compression, so a caller can perform that work outside the writer's sequential path.
    pub fn write_value(&mut self, id: &Id, value: EncodedValue) -> Result<()> {
        let location = if value.raw_length <= INLINE_BYTES as u64 {
            self.packs.account_inline(value.bytes.len() as u64)?;
            Location::Inline(value.bytes)
        } else {
            self.packs.append(*id, value)?
        };
        self.replace_location(id, location)
    }

    /// A normal write and a compaction copy make the same accounting change. The latter only
    /// replaces the physical address; the caller's records and logical versions stay intact.
    pub(crate) fn replace_location(&mut self, id: &Id, location: Location) -> Result<()> {
        if let Location::Pack {
            pack,
            offset,
            header,
        } = &location
        {
            let info = self.changed_packs.entry(*pack).or_insert(PackInfo {
                length: PackHeader::LENGTH,
                live_bytes: 0,
                live_values: 0,
                scan_offset: PackHeader::LENGTH,
            });
            info.length = offset + header.frame_length();
            info.live_bytes += header.frame_length();
            info.live_values += 1;
        }
        let previous = {
            let mut table = self.transaction.open_table(VALUES)?;
            let bytes = format::encode(&location)?;
            table
                .insert(id, bytes.as_slice())?
                .map(|previous| format::decode::<Location>(previous.value()))
                .transpose()?
        };
        if let Some(previous) = previous {
            self.retire(previous)?;
        }
        Ok(())
    }

    /// Read metadata from this checkpoint, including any pending changes.
    pub fn metadata(&self, id: &Id) -> Result<Option<Vec<u8>>> {
        let table = self.transaction.open_table(RECORDS)?;
        Ok(table.get(id)?.map(|bytes| bytes.value().to_vec()))
    }

    pub fn set_metadata(&self, id: &Id, bytes: &[u8]) -> Result<()> {
        self.transaction.open_table(RECORDS)?.insert(id, bytes)?;
        Ok(())
    }

    /// Store an immutable fragment referenced by this many records in the checkpoint.
    /// Returns None if the writer couldn't find a free ID and the caller should store it inline.
    pub fn share_metadata(&mut self, bytes: &[u8], references: u32) -> Result<Option<u32>> {
        assert!(references > 0, "shared metadata needs a referring record");
        let Some(ids) = &mut self.shared_ids else {
            return Ok(None);
        };
        let mut table = self.transaction.open_table(SHARED)?;
        if ids.is_empty() {
            let pick = || getrandom::u32().expect("supported platforms provide OS randomness");
            let Some(free) = Self::free_shared_ids(&table, pick)? else {
                self.shared_ids = None;
                return Ok(None);
            };
            *ids = free;
        }
        let id = u32::try_from(ids.start).expect("free ID fits u32");
        table.insert(id, (references, bytes))?;
        ids.start += 1;
        Ok(Some(id))
    }

    /// Usually we can issue IDs sequentially after the highest live ID. On overflow, pick a
    /// random u32 and ask the index for the first live ID at or above it. If we pick 1000
    /// and the index returns 1400, we can issue `1000..1400` with one lookup. The write
    /// transaction is exclusive, so that gap is ours to use.
    ///
    /// If we land on an occupied ID, try once more, then fall back to inline storage for
    /// this checkpoint. With 10 million live entries, even the first pick has a 99.77%
    /// chance of landing in a gap. The odds depend only on how many IDs are live today.
    fn free_shared_ids(
        table: &redb::Table<'_, u32, (u32, &[u8])>,
        mut pick: impl FnMut() -> u32,
    ) -> Result<Option<Range<u64>>> {
        let end = u64::from(u32::MAX) + 1;
        let tail = table
            .last()?
            .map_or(0, |(last, _)| u64::from(last.value()) + 1);
        if tail < end {
            return Ok(Some(tail..end));
        }

        for _ in 0..2 {
            let start = pick();
            let mut cursor = table.lower_bound(Bound::Included(start))?;
            let (next_live, _) = cursor.next()?.expect("u32::MAX is live");
            if start < next_live.value() {
                return Ok(Some(u64::from(start)..u64::from(next_live.value())));
            }
        }
        Ok(None)
    }

    /// Release the references owned by replaced or evicted records in this transaction.
    /// The last release removes the fragment along with its count.
    pub fn release_shared_metadata(
        &self,
        releases: impl Iterator<Item = (u32, u32)>,
    ) -> Result<()> {
        let mut releases = releases.peekable();
        if releases.peek().is_none() {
            return Ok(());
        }
        let mut table = self.transaction.open_table(SHARED)?;
        for (id, count) in releases {
            let remaining = {
                let entry = table
                    .get(id)?
                    .ok_or_else(|| Error::invalid_cache("missing shared metadata"))?;
                let (refs, bytes) = entry.value();
                let left = refs
                    .checked_sub(count)
                    .ok_or_else(|| Error::invalid_cache("invalid shared metadata count"))?;
                (left > 0).then(|| (left, bytes.to_vec()))
            };
            if let Some((left, bytes)) = remaining {
                table.insert(id, (left, bytes.as_slice()))?;
            } else {
                table.remove(id)?;
            }
        }
        Ok(())
    }

    pub fn remove_metadata(&self, id: &Id) -> Result<()> {
        self.transaction.open_table(RECORDS)?.remove(id)?;
        Ok(())
    }

    pub fn remove_value(&mut self, id: &Id) -> Result<()> {
        let previous = {
            let mut table = self.transaction.open_table(VALUES)?;
            table
                .remove(id)?
                .map(|previous| format::decode::<Location>(previous.value()))
                .transpose()?
        };
        if let Some(previous) = previous {
            self.retire(previous)?;
        }
        Ok(())
    }

    /// Removing a location makes its bytes dead, but cannot change the immutable file itself.
    /// When no locations remain, commit removes the catalog entry and cleanup can unlink it.
    fn retire(&mut self, location: Location) -> Result<()> {
        if let Location::Pack { pack, header, .. } = location {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                self.changed_packs.entry(pack)
            {
                let table = self.transaction.open_table(PACKS)?;
                let bytes = table
                    .get(&pack)?
                    .ok_or_else(|| Error::invalid_cache("missing pack inventory"))?;
                entry.insert(format::decode(bytes.value())?);
            }
            let info = self
                .changed_packs
                .get_mut(&pack)
                .expect("pack inventory loaded");
            info.live_bytes = info
                .live_bytes
                .checked_sub(header.frame_length())
                .ok_or_else(|| Error::invalid_cache("invalid pack byte count"))?;
            info.live_values = info
                .live_values
                .checked_sub(1)
                .ok_or_else(|| Error::invalid_cache("invalid pack value count"))?;
        }
        Ok(())
    }

    /// Publish the checkpoint atomically. If the outcome is uncertain, the returned error's
    /// [`requires_reopen`](Error::requires_reopen) method returns `true`.
    pub fn commit(mut self) -> Result<()> {
        // Sync new packs before committing an index that can reference them.
        let sync_time = self.packs.finish()?;
        let mut retired = Vec::new();
        {
            let mut table = self.transaction.open_table(PACKS)?;
            for (id, info) in &self.changed_packs {
                if info.live_values == 0 {
                    table.remove(id)?;
                    retired.push(*id);
                } else {
                    table.insert(id, format::encode(info)?.as_slice())?;
                }
            }
        }
        // Once commit is attempted, either checkpoint may be visible on failure. Leave pack
        // cleanup to reopening in that case so both possible checkpoints keep their files.
        self.packs.commit_attempted();
        let committing = Instant::now();
        if let Err(error) = self.transaction.commit() {
            self.database.healthy.store(false, Ordering::Release);
            return Err(Error::from(error).require_reopen());
        }
        let commit_time = committing.elapsed();
        if !self.changed_packs.is_empty() {
            self.database.compaction_due.store(true, Ordering::Relaxed);
        }
        if let Err(error) = self
            .database
            .finish_file_changes(self.packs.created_paths(), &retired)
        {
            self.database
                .reconciliation_due
                .store(true, Ordering::Relaxed);
            tracing::warn!(
                %error,
                cause = error.source().map(tracing::field::display),
                "file cleanup and accounting deferred"
            );
        }
        tracing::debug!(
            ?sync_time,
            ?commit_time,
            physical_bytes = self.database.physical_bytes(),
            "packed checkpoint committed"
        );
        Ok(())
    }
}

/// A consistent read view. Returned byte buffers remain usable after the view is dropped.
pub struct Read<'a> {
    pub(crate) transaction: redb::ReadTransaction,
    database: &'a Database,
    _guard: RwLockReadGuard<'a, ()>,
}

impl<'a> Read<'a> {
    pub(crate) fn new(database: &'a Database) -> Result<Self> {
        let guard = database.access.try_read().map_err(|_| {
            Error::access_conflict("finish the storage write before starting a read")
        })?;
        Ok(Self {
            transaction: database.index()?.begin_read()?,
            database,
            _guard: guard,
        })
    }

    pub fn identities(&self) -> Result<Vec<(String, u32, Vec<u8>)>> {
        self.transaction
            .open_table(IDENTITIES)?
            .iter()?
            .map(|row| {
                let (key, value) = row?;
                let (namespace, id) = key.value();
                Ok((namespace.to_owned(), id, value.value().to_vec()))
            })
            .collect()
    }

    /// Copy all metadata rows into an owned collection. Use [`visit_records`](Self::visit_records)
    /// to inspect a large catalog one row at a time.
    pub fn records(&self) -> Result<Vec<(Id, Vec<u8>)>> {
        let mut records = Vec::new();
        self.visit_records(|id, bytes| -> Result<()> {
            records.push((id, bytes.to_vec()));
            Ok(())
        })?;
        Ok(records)
    }

    /// Visit each record's metadata while its index page is borrowed. An error from the
    /// visitor stops the scan and lets the caller report a problem with its own record format.
    pub fn visit_records<E: From<Error>>(
        &self,
        mut visit: impl FnMut(Id, &[u8]) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        let records = self.transaction.open_table(RECORDS).map_err(Error::from)?;
        for row in records.iter().map_err(Error::from)? {
            let (id, bytes) = row.map_err(Error::from)?;
            visit(*id.value(), bytes.value())?;
        }
        Ok(())
    }

    pub fn metadata(&self, id: &Id) -> Result<Option<Vec<u8>>> {
        let table = self.transaction.open_table(RECORDS)?;
        Ok(table.get(id)?.map(|bytes| bytes.value().to_vec()))
    }

    /// Read one referenced fragment without loading the other fragments in the checkpoint.
    pub fn shared_metadata(&self, id: u32) -> Result<Vec<u8>> {
        let table = self.transaction.open_table(SHARED)?;
        let entry = table
            .get(id)?
            .ok_or_else(|| Error::invalid_cache("missing shared metadata"))?;
        Ok(entry.value().1.to_vec())
    }

    /// Read one value as owned, uncompressed bytes without reading other payloads.
    /// A missing or malformed value reports [`crate::ErrorKind::InvalidCache`].
    pub fn read_value(&self, id: &Id, length: u64) -> Result<Vec<u8>> {
        let location: Location = {
            let table = self.transaction.open_table(VALUES)?;
            let value = table
                .get(id)?
                .ok_or_else(|| Error::invalid_cache("missing stored value"))?;
            format::decode(value.value())?
        };
        match location {
            Location::Inline(bytes) => {
                ensure!(
                    bytes.len() as u64 == length,
                    Error::invalid_cache("invalid inline value length"),
                );
                Ok(bytes)
            }
            Location::Pack {
                pack,
                offset,
                header,
            } => {
                ensure!(
                    &header.id == id && header.raw_length == length,
                    Error::invalid_cache("invalid value identity or length"),
                );
                let table = self.transaction.open_table(PACKS)?;
                let info: PackInfo = format::decode(
                    table
                        .get(&pack)?
                        .ok_or_else(|| Error::invalid_cache("missing pack inventory"))?
                        .value(),
                )?;
                let end = offset
                    .checked_add(header.frame_length())
                    .ok_or_else(|| Error::invalid_cache("invalid pack range"))?;
                ensure!(
                    offset >= PackHeader::LENGTH && end <= info.length,
                    Error::invalid_cache("invalid pack range"),
                );
                let file = {
                    let mut files = self.database.files.lock().expect("pack handles");
                    if let Some((_, file)) = files.iter().find(|(id, _)| id == &pack) {
                        file.clone()
                    } else {
                        let file = Arc::new(self.database.open_pack(&pack)?);
                        if files.len() == MAX_CACHED_PACK_HANDLES {
                            files.remove(0);
                        }
                        files.push((pack, file.clone()));
                        file
                    }
                };
                let mut framing = [0; FrameHeader::LENGTH as usize];
                Self::read_exact_at(&file, &mut framing, offset)?;
                let frame: FrameHeader = format::decode(&framing)?;
                ensure!(
                    frame == header,
                    Error::invalid_cache("invalid stored frame header"),
                );
                // Read the payload into its own allocation, avoiding a full-value memmove to
                // remove a framing prefix before returning an uncompressed value.
                let stored_length =
                    usize::try_from(header.stored_length).map_err(Error::invalid_source)?;
                let mut bytes = vec![0; stored_length];
                Self::read_exact_at(&file, &mut bytes, offset + FrameHeader::LENGTH)?;
                ensure!(
                    crc32c::crc32c(&bytes) == header.checksum,
                    Error::invalid_cache("invalid stored value checksum"),
                );
                header.compression.decode(bytes, length)
            }
        }
    }

    fn read_exact_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            file.read_exact_at(bytes, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut remaining = bytes;
            let mut offset = offset;
            while !remaining.is_empty() {
                let read = file.seek_read(remaining, offset)?;
                if read == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                offset += read as u64;
                remaining = &mut remaining[read..];
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SHARED, Write};
    use crate::{Database, Options, Result};

    #[test]
    fn shared_ids_use_the_last_id_then_probe_for_a_free_range() -> Result<()> {
        let root = tempfile::tempdir()?;
        let database = Database::open(root.path(), "shared IDs", Options::default())?;
        let mut write = database.begin_write()?;
        {
            // Seed the high end so reaching overflow takes one ordinary allocation.
            let mut table = write.transaction.open_table(SHARED)?;
            for id in [1400, u32::MAX - 1] {
                table.insert(id, (1, b"live".as_slice()))?;
            }
        }
        assert_eq!(write.share_metadata(b"last ID", 1)?, Some(u32::MAX));

        let table = write.transaction.open_table(SHARED)?;
        for (picks, expected, attempts) in [
            ([1000, 0], Some(1000..1400), 1),
            ([1400, 1399], Some(1399..1400), 2),
            ([u32::MAX, 1400], None, 2),
        ] {
            let mut picks = picks.into_iter();
            let free =
                Write::free_shared_ids(&table, || picks.next().expect("at most two probes"))?;
            assert_eq!(free, expected);
            assert_eq!(2 - picks.len(), attempts);
        }
        Ok(())
    }
}
