//! Exclusive ownership, opening, and maintenance of one cache directory.
//!
//! The sidecar lock remains held even if an unreadable index has to be replaced. A second
//! process can then choose another instance without racing cleanup of this one's pack files.
//! Inside the process, read guards keep old packs alive until their reads finish.

use std::{
    collections::HashSet,
    fmt,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use anyhow::Result;
use redb::{ReadableDatabase, ReadableTable};

use crate::{
    Compression, Read, Statistics, Write,
    format::{self, HEADER, IDENTITIES, PACKS, PackHeader, PackId, PackInfo, RECORDS, VALUES},
    pack::PackWriter,
};

/// Another owner already holds this cache directory. The host can select another instance.
#[derive(Debug)]
pub struct DatabaseInUse;

impl fmt::Display for DatabaseInUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("query storage is already open")
    }
}
impl std::error::Error for DatabaseInUse {}

/// A value write exhausted the storage budget. Existing values can still be read, and removing
/// results or compacting remains possible. Unlike a failed commit, this does not poison storage.
#[derive(Debug)]
pub struct StorageFull;

impl fmt::Display for StorageFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("query storage is full; compact it or discard cached results")
    }
}
impl std::error::Error for StorageFull {}

/// Policy for writes and maintenance; existing frames carry their own decoding information.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub compression: Compression,
    /// Physical budget, including obsolete files and the index. Normal payload writes leave
    /// room for one compaction pass. This is a soft limit: index growth within a checkpoint or
    /// moving one oversized frame can exceed it. Maintenance and removals remain available.
    pub max_physical_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            compression: Compression::Lz4,
            max_physical_bytes: 32 * 1024 * 1024 * 1024,
        }
    }
}

/// Owns the index and all payload files of one disposable cache.
///
/// Reads borrow this owner, and a write excludes reads until publication and file retirement
/// finish. Callers should keep these scopes short and retain decoded values instead of readers.
pub struct Database {
    pub(crate) index: redb::Database,
    pub(crate) root: PathBuf,
    pub(crate) packs: PathBuf,
    pub(crate) instance: PackId,
    pub(crate) access: RwLock<()>,
    pub(crate) healthy: AtomicBool,
    pub(crate) physical_bytes: AtomicU64,
    pub(crate) maintenance_due: AtomicBool,
    pub(crate) options: Options,
    // A handful of ordinary file handles amortizes opens. No mappings or decoded values live
    // here. Writes clear the cache while holding exclusive access before unlinking old files.
    pub(crate) files: Mutex<Vec<(PackId, std::sync::Arc<File>)>>,
    _owner: File,
}

impl Database {
    /// Open an instance, resetting incompatible or unreadable contents while still owning its
    /// directory lock. Format changes discard derivable data; no schema migration is attempted.
    pub fn open(path: &Path, epoch: &str, options: Options) -> Result<Self> {
        fs::create_dir_all(path)?;
        let owner = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.join("owner.lock"))?;
        owner.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => anyhow::Error::from(DatabaseInUse),
            fs::TryLockError::Error(error) => error.into(),
        })?;
        let packs = path.join("packs");
        fs::create_dir_all(&packs)?;
        let opened = Self::open_contents(path, epoch);
        let (index, instance) = match opened {
            Ok(contents) => contents,
            Err(error)
                if matches!(
                    error.downcast_ref::<redb::DatabaseError>(),
                    Some(redb::DatabaseError::DatabaseAlreadyOpen)
                ) =>
            {
                return Err(DatabaseInUse.into());
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "discarding unreadable packed storage");
                // The failed open has released all redb handles. Keep owner.lock in place so
                // another opener cannot acquire a new lock inode while this reset is running.
                let index = path.join("index.redb");
                if index.exists() {
                    fs::remove_file(index)?;
                }
                fs::remove_dir_all(&packs)?;
                fs::create_dir_all(&packs)?;
                Self::open_contents(path, epoch)?
            }
        };
        let database = Self {
            index,
            root: path.to_owned(),
            packs,
            instance,
            access: RwLock::new(()),
            healthy: AtomicBool::new(true),
            physical_bytes: AtomicU64::new(0),
            maintenance_due: AtomicBool::new(true),
            options,
            files: Mutex::default(),
            _owner: owner,
        };
        if let Err(error) = database.clean_orphans() {
            tracing::warn!(%error, "pack cleanup deferred");
        }
        database.refresh_physical_bytes()?;
        Ok(database)
    }

    fn open_contents(path: &Path, epoch: &str) -> Result<(redb::Database, PackId)> {
        let index = redb::Database::builder()
            .set_cache_size(8 * 1024 * 1024)
            .create(path.join("index.redb"))?;
        let expected = format!("{}:{epoch}", format::FORMAT);
        let write = index.begin_write()?;
        let (reset, instance) = {
            let mut header = write.open_table(HEADER)?;
            let reset = header
                .get("epoch")?
                .is_none_or(|value| value.value() != expected.as_bytes());
            let instance = if reset {
                let instance = format::new_id()?;
                header.insert("epoch", expected.as_bytes())?;
                header.insert("instance", instance.as_slice())?;
                instance
            } else {
                header
                    .get("instance")?
                    .ok_or_else(|| anyhow::anyhow!("missing storage instance"))?
                    .value()
                    .try_into()?
            };
            (reset, instance)
        };
        if reset {
            write.delete_table(RECORDS)?;
            write.delete_table(IDENTITIES)?;
            write.delete_table(VALUES)?;
            write.delete_table(PACKS)?;
        }
        drop(write.open_table(RECORDS)?);
        drop(write.open_table(IDENTITIES)?);
        drop(write.open_table(VALUES)?);
        drop(write.open_table(PACKS)?);
        write.commit()?;

        // Opening checks existence and framing, not every payload. Individual reads verify
        // checksums later, so restarting does not scan gigabytes of cached values.
        let read = index.begin_read()?;
        for entry in read.open_table(PACKS)?.iter()? {
            let (id, bytes) = entry?;
            let info: PackInfo = wincode::deserialize_exact(bytes.value())?;
            anyhow::ensure!(
                info.length >= PackHeader::LENGTH
                    && info.live_bytes <= info.length - PackHeader::LENGTH
                    && info.live_values > 0
                    && (PackHeader::LENGTH..info.length).contains(&info.scan_offset),
                "invalid pack inventory"
            );
            let mut file = File::open(PackWriter::path(&path.join("packs"), id.value()))?;
            anyhow::ensure!(
                file.metadata()?.len() == info.length,
                "invalid pack file length"
            );
            let mut bytes = [0; PackHeader::LENGTH as usize];
            std::io::Read::read_exact(&mut file, &mut bytes)?;
            let header: PackHeader = wincode::deserialize_exact(&bytes)?;
            anyhow::ensure!(
                header
                    == PackHeader {
                        magic: PackHeader::MAGIC,
                        instance,
                        pack: *id.value()
                    },
                "invalid pack identity"
            );
        }
        drop(read);
        PackWriter::sync_directory(path)?;
        Ok((index, instance))
    }

    pub fn compression(&self) -> Compression {
        self.options.compression
    }

    pub fn set_compression(&mut self, compression: Compression) {
        self.options.compression = compression;
    }

    /// Keep a consistent index view and pin its packs until the reader drops.
    pub fn snapshot(&self) -> Result<Read<'_>> {
        Read::new(self)
    }

    /// Begin one checkpoint. Drop any reader first: publication needs exclusive access so old
    /// pack files can be removed as soon as their last committed location is replaced.
    pub fn begin_write(&self) -> Result<Write<'_>> {
        Write::new(self)
    }

    pub(crate) fn check(&self) -> Result<()> {
        anyhow::ensure!(
            self.healthy.load(Ordering::Acquire),
            "reopen storage after its failed commit"
        );
        Ok(())
    }

    /// Drop query records and values together. Stable identities survive ordinary invalidation;
    /// a host resetting those too must first discard every captured input using the old IDs.
    pub fn invalidate(&mut self, reset_identities: bool) -> Result<()> {
        let mut write = self.begin_write()?;
        write.clear(reset_identities)?;
        write.commit()?;
        self.healthy.store(true, Ordering::Release);
        Ok(())
    }

    /// Finish pack reclamation and shrink the index. This explicit idle operation is unbounded;
    /// ordinary query operations use the small passes in [`Self::maintain`] instead.
    pub fn compact(&mut self) -> Result<bool> {
        let mut changed = false;
        while self.compact_step(true)? {
            changed = true;
        }
        self.files.get_mut().expect("pack handles").clear();
        self.clean_orphans()?;
        changed |= self.index.compact()?;
        self.refresh_physical_bytes()?;
        Ok(changed)
    }

    pub(crate) fn write_budget(&self) -> u64 {
        let headroom = (self.options.max_physical_bytes / 8).min(super::maintenance::PASS_BYTES);
        self.options
            .max_physical_bytes
            .saturating_sub(headroom)
            .saturating_sub(self.physical_bytes.load(Ordering::Relaxed))
    }

    /// Remove files that neither a successful checkpoint nor its live values can reach.
    /// This is used only while opening, or under exclusive access after publication.
    pub(crate) fn clean_orphans(&self) -> Result<()> {
        let read = self.index.begin_read()?;
        let mut keep = HashSet::new();
        for entry in read.open_table(PACKS)?.iter()? {
            let (id, _) = entry?;
            keep.insert(PackWriter::path(&self.packs, id.value()));
        }
        for entry in fs::read_dir(&self.packs)? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "pack")
                && !keep.contains(&path)
            {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    pub(crate) fn allocated(metadata: &fs::Metadata) -> u64 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            metadata.blocks() * 512
        }
        #[cfg(not(unix))]
        {
            metadata.len()
        }
    }

    pub(crate) fn refresh_physical_bytes(&self) -> Result<()> {
        let mut bytes = Self::allocated(&fs::metadata(self.root.join("index.redb"))?);
        for entry in fs::read_dir(&self.packs)? {
            bytes += Self::allocated(&entry?.metadata()?);
        }
        self.physical_bytes.store(bytes, Ordering::Relaxed);
        Ok(())
    }

    /// Inspect contents without decoding query payloads. This scans index rows and file metadata,
    /// so callers should use it for diagnostics rather than after every small request.
    pub fn statistics(&self) -> Result<Statistics> {
        let read = self.snapshot()?;
        Statistics::read_index(&read.transaction, &self.root)
    }
}
