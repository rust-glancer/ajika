//! Read complete values, or publish a batch of replacements.
//!
//! The index is the only publication authority. New frames may already exist on disk while a
//! write is pending, but no reader can find them through the old index. Pack accounting changes
//! in the same commit as locations, so deleting the last value can retire its entire file.

use std::{
    collections::BTreeMap,
    fs::File,
    io,
    sync::{Arc, RwLockReadGuard, RwLockWriteGuard, atomic::Ordering},
    time::Instant,
};

use anyhow::Result;
use redb::{ReadableDatabase, ReadableTable};

use crate::{
    Database, EncodedValue, Id,
    format::{
        FrameHeader, IDENTITIES, Location, PACKS, PackHeader, PackId, PackInfo, RECORDS, VALUES,
    },
    pack::PackWriter,
    value::INLINE_BYTES,
};

/// A checkpoint under construction. Dropping it before commit discards its index changes and
/// provisional files. Reads must finish before beginning a write on the same database.
pub struct Write<'a> {
    pub(crate) transaction: redb::WriteTransaction,
    database: &'a Database,
    pub(crate) packs: PackWriter,
    pub(crate) changed_packs: BTreeMap<PackId, PackInfo>,
    _guard: RwLockWriteGuard<'a, ()>,
}

impl<'a> Write<'a> {
    pub(crate) fn new(database: &'a Database) -> Result<Self> {
        Self::with_budget(database, database.write_budget())
    }

    /// Maintenance is bounded by its scan/copy pass, not by the exhausted normal-write budget.
    /// It must still be able to release space after index growth crosses that soft limit.
    pub(crate) fn maintenance(database: &'a Database) -> Result<Self> {
        Self::with_budget(database, u64::MAX)
    }

    fn with_budget(database: &'a Database, remaining: u64) -> Result<Self> {
        let guard = database
            .access
            .try_write()
            .map_err(|_| anyhow::anyhow!("finish storage reads before starting a write"))?;
        database.check()?;
        Ok(Self {
            transaction: database.index.begin_write()?,
            database,
            packs: PackWriter::new(database.packs.clone(), database.instance, remaining),
            changed_packs: BTreeMap::new(),
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
            let bytes = wincode::serialize(&location)?;
            table
                .insert(id, bytes.as_slice())?
                .map(|previous| wincode::deserialize_exact::<Location>(previous.value()))
                .transpose()?
        };
        if let Some(previous) = previous {
            self.retire(previous)?;
        }
        Ok(())
    }

    pub fn set_metadata(&self, id: &Id, bytes: &[u8]) -> Result<()> {
        self.transaction.open_table(RECORDS)?.insert(id, bytes)?;
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
                .map(|previous| wincode::deserialize_exact::<Location>(previous.value()))
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
                    .ok_or_else(|| anyhow::anyhow!("missing pack inventory"))?;
                entry.insert(wincode::deserialize_exact(bytes.value())?);
            }
            let info = self
                .changed_packs
                .get_mut(&pack)
                .expect("pack inventory loaded");
            info.live_bytes = info
                .live_bytes
                .checked_sub(header.frame_length())
                .ok_or_else(|| anyhow::anyhow!("invalid pack byte count"))?;
            info.live_values = info
                .live_values
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("invalid pack value count"))?;
        }
        Ok(())
    }

    pub(crate) fn clear(&mut self, reset_identities: bool) -> Result<()> {
        self.transaction.delete_table(RECORDS)?;
        self.transaction.delete_table(VALUES)?;
        self.transaction.delete_table(PACKS)?;
        self.changed_packs.clear();
        if reset_identities {
            self.transaction.delete_table(IDENTITIES)?;
        }
        drop(self.transaction.open_table(RECORDS)?);
        drop(self.transaction.open_table(VALUES)?);
        drop(self.transaction.open_table(PACKS)?);
        drop(self.transaction.open_table(IDENTITIES)?);
        Ok(())
    }

    /// Synchronize new files before publishing their locations. A commit error is ambiguous:
    /// the database may contain the new checkpoint, so leave files intact and require reopen.
    pub fn commit(mut self) -> Result<()> {
        let sync_time = self.packs.finish()?;
        {
            let mut table = self.transaction.open_table(PACKS)?;
            for (id, info) in &self.changed_packs {
                if info.live_values == 0 {
                    table.remove(id)?;
                } else {
                    table.insert(id, wincode::serialize(info)?.as_slice())?;
                }
            }
        }
        self.packs.commit_attempted();
        let committing = Instant::now();
        if let Err(error) = self.transaction.commit() {
            self.database.healthy.store(false, Ordering::Release);
            return Err(error.into());
        }
        let commit_time = committing.elapsed();
        if !self.changed_packs.is_empty() {
            self.database.maintenance_due.store(true, Ordering::Relaxed);
        }
        // No reader or cached handle may keep an obsolete file open when it is unlinked.
        self.database.files.lock().expect("pack handles").clear();
        if let Err(error) = self.database.clean_orphans() {
            tracing::warn!(%error, "pack cleanup deferred until reopen");
        }
        if let Err(error) = self.database.refresh_physical_bytes() {
            // If accounting is unavailable, stop accepting writes rather than bypass the limit.
            self.database
                .physical_bytes
                .store(u64::MAX, Ordering::Relaxed);
            tracing::warn!(%error, "could not measure packed cache size");
        }
        tracing::debug!(
            ?sync_time,
            ?commit_time,
            physical_bytes = self.database.physical_bytes.load(Ordering::Relaxed),
            "packed checkpoint committed"
        );
        Ok(())
    }
}

/// A short consistent view. Returned byte buffers do not retain the transaction or file handles.
pub struct Read<'a> {
    pub(crate) transaction: redb::ReadTransaction,
    database: &'a Database,
    _guard: RwLockReadGuard<'a, ()>,
}

impl<'a> Read<'a> {
    pub(crate) fn new(database: &'a Database) -> Result<Self> {
        let guard = database
            .access
            .try_read()
            .map_err(|_| anyhow::anyhow!("finish the storage write before starting a read"))?;
        database.check()?;
        Ok(Self {
            transaction: database.index.begin_read()?,
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

    /// Enumerate records without loading payloads. The caller interprets their contents.
    pub fn records(&self) -> Result<Vec<(Id, Vec<u8>)>> {
        self.transaction
            .open_table(RECORDS)?
            .iter()?
            .map(|entry| {
                let (key, value) = entry?;
                Ok((*key.value(), value.value().to_vec()))
            })
            .collect()
    }

    pub fn metadata(&self, id: &Id) -> Result<Option<Vec<u8>>> {
        let table = self.transaction.open_table(RECORDS)?;
        Ok(table.get(id)?.map(|bytes| bytes.value().to_vec()))
    }

    /// Read and validate one frame, then decode its compression. Integrity is checked before
    /// the caller's deserializer sees bytes. Other values in the pack are not read or decoded.
    pub fn read_value(&self, id: &Id, length: u64) -> Result<Vec<u8>> {
        let location: Location = {
            let table = self.transaction.open_table(VALUES)?;
            let value = table
                .get(id)?
                .ok_or_else(|| anyhow::anyhow!("missing stored value"))?;
            wincode::deserialize_exact(value.value())?
        };
        match location {
            Location::Inline(bytes) => {
                anyhow::ensure!(bytes.len() as u64 == length, "invalid inline value length");
                Ok(bytes)
            }
            Location::Pack {
                pack,
                offset,
                header,
            } => {
                anyhow::ensure!(
                    &header.id == id && header.raw_length == length,
                    "invalid value identity or length"
                );
                let table = self.transaction.open_table(PACKS)?;
                let info: PackInfo = wincode::deserialize_exact(
                    table
                        .get(&pack)?
                        .ok_or_else(|| anyhow::anyhow!("missing pack inventory"))?
                        .value(),
                )?;
                let end = offset
                    .checked_add(header.frame_length())
                    .ok_or_else(|| anyhow::anyhow!("invalid pack range"))?;
                anyhow::ensure!(
                    offset >= PackHeader::LENGTH && end <= info.length,
                    "invalid pack range"
                );
                let file = {
                    let mut files = self.database.files.lock().expect("pack handles");
                    if let Some((_, file)) = files.iter().find(|(id, _)| id == &pack) {
                        file.clone()
                    } else {
                        let file =
                            Arc::new(File::open(PackWriter::path(&self.database.packs, &pack))?);
                        if files.len() == 16 {
                            files.remove(0);
                        }
                        files.push((pack, file.clone()));
                        file
                    }
                };
                let mut framing = [0; FrameHeader::LENGTH as usize];
                Self::read_exact_at(&file, &mut framing, offset)?;
                let frame: FrameHeader = wincode::deserialize_exact(&framing)?;
                anyhow::ensure!(frame == header, "invalid stored frame header");
                // Read the payload into its own allocation, avoiding a full-value memmove to
                // remove a framing prefix before returning an uncompressed value.
                let mut bytes = vec![0; usize::try_from(header.stored_length)?];
                Self::read_exact_at(&file, &mut bytes, offset + FrameHeader::LENGTH)?;
                anyhow::ensure!(
                    blake3::hash(&bytes).as_bytes() == &header.checksum,
                    "invalid stored value checksum"
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
