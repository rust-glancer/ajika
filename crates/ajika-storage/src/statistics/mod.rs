//! Measure retained data and actual files at the same boundary.
//!
//! Logical bytes describe what callers can reuse. Physical bytes also include framing, dead
//! payload ranges, redb pages, and files left by interrupted cleanup. Keeping those separate
//! prevents a replacement from appearing to reclaim disk space that it still occupies.

use std::{
    fs::{self, File},
    path::Path,
};

use redb::{ReadableDatabase, ReadableTable};

use crate::{
    Database, Error, Result,
    format::{self, IDENTITIES, Location, PACKS, PackHeader, PackInfo, RECORDS, VALUES},
};

/// Logical contents and filesystem usage at the same observation boundary.
/// Apparent lengths and allocated bytes differ on filesystems that support sparse files.
#[derive(Debug, Default, Clone, Copy)]
pub struct Statistics {
    pub records: u64,
    pub record_bytes: u64,
    pub identity_bytes: u64,
    pub location_bytes: u64,
    pub values: u64,
    pub raw_value_bytes: u64,
    pub inline_bytes: u64,
    pub inline_values: u64,
    pub largest_value_bytes: u64,
    /// Raw sizes: up to 1 KiB, 16 KiB, 64 KiB, 1 MiB, 16 MiB, then larger.
    pub value_size_counts: [u64; 6],
    pub packed_value_bytes: u64,
    pub dead_pack_bytes: u64,
    pub pack_files: u64,
    pub pack_file_bytes: u64,
    pub index_file_bytes: u64,
    pub allocated_bytes: u64,
}

impl Statistics {
    /// Inspect a closed cache without changing its epoch, repairing its index, or deleting files.
    pub fn inspect(path: &Path) -> Result<Self> {
        let owner = File::open(path.join("owner.lock"))?;
        owner.try_lock_shared().map_err(|error| match error {
            fs::TryLockError::WouldBlock => Error::in_use(),
            fs::TryLockError::Error(error) => error.into(),
        })?;
        let index = redb::Database::builder()
            .set_cache_size(8 * 1024 * 1024)
            .open_read_only(path.join("index.redb"))?;
        Self::read_index(&index.begin_read()?, path)
    }

    pub(crate) fn read_index(read: &redb::ReadTransaction, path: &Path) -> Result<Self> {
        let mut stats = Statistics::default();
        for entry in read.open_table(RECORDS)?.iter()? {
            let (_, value) = entry?;
            stats.records += 1;
            stats.record_bytes += value.value().len() as u64;
        }
        for entry in read.open_table(IDENTITIES)?.iter()? {
            let (key, value) = entry?;
            stats.identity_bytes += key.value().0.len() as u64 + 4 + value.value().len() as u64;
        }
        for entry in read.open_table(VALUES)?.iter()? {
            let (_, value) = entry?;
            stats.values += 1;
            stats.location_bytes += value.value().len() as u64;
            let raw_length = match format::decode::<Location>(value.value())? {
                Location::Inline(bytes) => {
                    stats.inline_bytes += bytes.len() as u64;
                    stats.inline_values += 1;
                    bytes.len() as u64
                }
                Location::Pack { header, .. } => {
                    stats.packed_value_bytes += header.stored_length;
                    header.raw_length
                }
            };
            stats.raw_value_bytes += raw_length;
            stats.largest_value_bytes = stats.largest_value_bytes.max(raw_length);
            let bucket = [1024, 16 * 1024, 64 * 1024, 1024 * 1024, 16 * 1024 * 1024]
                .partition_point(|limit| *limit < raw_length);
            stats.value_size_counts[bucket] += 1;
        }
        for entry in read.open_table(PACKS)?.iter()? {
            let (_, value) = entry?;
            let info: PackInfo = format::decode(value.value())?;
            stats.dead_pack_bytes += info.length - PackHeader::LENGTH - info.live_bytes;
        }
        let index = fs::metadata(path.join("index.redb"))?;
        stats.index_file_bytes = index.len();
        stats.allocated_bytes = Database::allocated(&index);
        for entry in fs::read_dir(path.join("packs"))? {
            let metadata = entry?.metadata()?;
            stats.pack_files += 1;
            stats.pack_file_bytes += metadata.len();
            stats.allocated_bytes += Database::allocated(&metadata);
        }
        Ok(stats)
    }
}
