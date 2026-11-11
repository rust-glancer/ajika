//! Exclusive ownership, opening, and maintenance of one cache directory.
//!
//! The sidecar lock remains held even if an unreadable index has to be replaced. A second
//! process can then choose another instance without racing cleanup of this one's pack files.
//! Inside the process, read guards keep old packs alive until their reads finish.

use std::{
    collections::HashSet,
    error::Error as _,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use redb::{ReadableDatabase, ReadableTable};

use crate::{
    Compression, Error, ErrorKind, Read, Result, Statistics, Write,
    error::ensure,
    format::{
        self, HEADER, IDENTITIES, InstanceId, PACKS, PackHeader, PackId, PackInfo, RECORDS, VALUES,
    },
    pack::PackWriter,
};

const INDEX_FILE: &str = "index.redb";
const INDEX_CACHE_BYTES: usize = 8 * 1024 * 1024;
const REBUILD_FILE: &str = "index.rebuild.redb";

#[derive(Clone, Copy)]
struct DiskUsage {
    index_bytes: u64,
    pack_bytes: u64,
}

/// A key and its assigned ID within a namespace. Pass existing assignments to
/// [`Database::rebuild`] so callers can keep using their IDs after an index is replaced.
#[derive(Debug, Clone, Copy)]
pub struct Identity<'a> {
    pub namespace: &'a str,
    pub id: u32,
    pub key: &'a [u8],
}

/// Policy for writes and maintenance; existing frames carry their own decoding information.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub compression: Compression,
    /// Soft limit on disk usage, including obsolete files and the index.
    /// Maintenance and removals remain available when the limit is exceeded.
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
    // Recovery can drop the redb handle while `_owner` keeps the directory locked.
    index: Option<redb::Database>,
    epoch: String,
    pub(crate) root: PathBuf,
    pub(crate) packs: PathBuf,
    pub(crate) instance: InstanceId,
    pub(crate) access: RwLock<()>,
    pub(crate) healthy: AtomicBool,
    // Unknown totals require reconciliation before a writer can calculate its budget.
    disk_usage: Mutex<Option<DiskUsage>>,
    pub(crate) reconciliation_due: AtomicBool,
    pub(crate) compaction_due: AtomicBool,
    pub(crate) options: Options,
    // A handful of ordinary file handles amortizes opens. No mappings or decoded values live
    // here. Writes clear the cache while holding exclusive access before unlinking old files.
    pub(crate) files: Mutex<Vec<(PackId, std::sync::Arc<File>)>>,
    _owner: File,
}

impl Database {
    /// Open the cache and hold its directory lock for as long as the owner is alive.
    /// Incompatible or confirmed corrupt contents can be discarded under that lock.
    /// Keep only data the application can rebuild in this directory.
    pub fn open(path: &Path, epoch: &str, options: Options) -> Result<Self> {
        let result = (|| -> Result<Self> {
            fs::create_dir_all(path)?;
            let owner = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(path.join("owner.lock"))?;
            owner.try_lock().map_err(|error| match error {
                fs::TryLockError::WouldBlock => Error::in_use(),
                fs::TryLockError::Error(error) => error.into(),
            })?;
            let mut database = Self {
                index: None,
                epoch: format!("{}:{epoch}", format::FORMAT),
                root: path.to_owned(),
                packs: path.join("packs"),
                instance: InstanceId([0; 16]),
                access: RwLock::new(()),
                healthy: AtomicBool::new(false),
                disk_usage: Mutex::default(),
                reconciliation_due: AtomicBool::new(true),
                compaction_due: AtomicBool::new(true),
                options,
                files: Mutex::default(),
                _owner: owner,
            };

            if let Err(error) = database.load(true) {
                if error.kind() != ErrorKind::InvalidCache {
                    return Err(error);
                }
                // Pack damage can leave the identity table intact. Try clearing query data
                // first so callers can still load those assignments.
                let identities_discarded = match database.reset(None) {
                    Ok(()) => false,
                    Err(reset) if reset.kind() == ErrorKind::InvalidCache => {
                        database.reset(Some(&[]))?;
                        true
                    }
                    Err(reset) => return Err(reset),
                };
                tracing::warn!(
                    %error,
                    cause = error.source().map(tracing::field::display),
                    path = %path.display(),
                    identities_discarded,
                    "discarded corrupt cache contents"
                );
            }
            Ok(database)
        })();
        result.map_err(|error| error.at(path))
    }

    /// Reload the disk checkpoint while retaining the directory lock.
    ///
    /// After an uncertain commit, the recovered index tells us which packs to keep.
    /// If reopening fails, reads and writes resume after a successful reopen or rebuild.
    pub fn reopen(&mut self) -> Result<()> {
        self.load(false)
            .map_err(|error| error.require_reopen().at(&self.root))
    }

    fn load(&mut self, create: bool) -> Result<()> {
        self.load_index(create)?;
        self.validate_catalog()?;
        self.finish_open()
    }

    fn load_index(&mut self, create: bool) -> Result<()> {
        self.healthy.store(false, Ordering::Release);
        *self.disk_usage.get_mut().expect("disk usage") = None;
        self.reconciliation_due.store(true, Ordering::Relaxed);
        self.files.get_mut().expect("pack handles").clear();
        self.index.take();

        let path = self.root.join(INDEX_FILE);
        let mut builder = redb::Database::builder();
        builder.set_cache_size(INDEX_CACHE_BYTES);
        let index = if create {
            builder.create(&path)?
        } else {
            // Callers may already hold IDs from this cache. A missing index needs recovery
            // so the caller can republish those assignments.
            builder.open(&path).map_err(|error| match error {
                redb::DatabaseError::Storage(redb::StorageError::Io(source))
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    Error::invalid_source(source)
                }
                error => error.into(),
            })?
        };
        self.instance = match self.read_index_instance(&index)? {
            Some(instance) => instance,
            None if create => self.initialize_index(&index, &[])?,
            None => return Err(Error::invalid_cache("storage epoch changed")),
        };
        self.index = Some(index);
        Ok(())
    }

    fn read_index_instance(&self, index: &redb::Database) -> Result<Option<InstanceId>> {
        let read = index.begin_read()?;
        let header = match read.open_table(HEADER) {
            Ok(header) => header,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if header
            .get("epoch")?
            .is_none_or(|value| value.value() != self.epoch.as_bytes())
        {
            return Ok(None);
        }
        let instance = InstanceId(
            header
                .get("instance")?
                .ok_or_else(|| Error::invalid_cache("missing storage instance"))?
                .value()
                .try_into()
                .map_err(Error::invalid_source)?,
        );
        Ok(Some(instance))
    }

    fn initialize_index(
        &self,
        index: &redb::Database,
        identities: &[Identity<'_>],
    ) -> Result<InstanceId> {
        let instance = InstanceId(format::new_id()?);
        let write = index.begin_write()?;
        {
            let mut header = write.open_table(HEADER)?;
            header.insert("epoch", self.epoch.as_bytes())?;
            header.insert("instance", instance.0.as_slice())?;
        }
        // Commit the epoch and cleared catalog together so a restart sees a matching pair.
        Self::clear_catalog(&write, Some(identities))?;
        write.commit()?;
        Ok(instance)
    }

    fn validate_catalog(&self) -> Result<()> {
        let index = self.index.as_ref().expect("loaded index");
        let read = index.begin_read()?;
        drop(read.open_table(IDENTITIES)?);
        drop(read.open_table(RECORDS)?);
        drop(read.open_table(VALUES)?);
        // Check the files and their headers up front. Verifying each payload on demand
        // keeps opening a large cache from reading gigabytes of saved results.
        for entry in read.open_table(PACKS)?.iter()? {
            let (id, bytes) = entry?;
            let info: PackInfo = format::decode(bytes.value())?;
            ensure!(
                info.length >= PackHeader::LENGTH
                    && info.live_bytes <= info.length - PackHeader::LENGTH
                    && info.live_values > 0
                    && (PackHeader::LENGTH..info.length).contains(&info.scan_offset),
                Error::invalid_cache("invalid pack inventory"),
            );
            let mut file = self.open_pack(id.value())?;
            ensure!(
                file.metadata()?.len() == info.length,
                Error::invalid_cache("invalid pack file length"),
            );
            let mut bytes = [0; PackHeader::LENGTH as usize];
            std::io::Read::read_exact(&mut file, &mut bytes)?;
            let header: PackHeader = format::decode(&bytes)?;
            let expected_header = PackHeader {
                magic: PackHeader::MAGIC,
                instance: self.instance,
                pack: *id.value(),
            };
            ensure!(
                header == expected_header,
                Error::invalid_cache("invalid pack identity"),
            );
        }
        Ok(())
    }

    fn finish_open(&mut self) -> Result<()> {
        fs::create_dir_all(&self.packs)?;
        self.files.get_mut().expect("pack handles").clear();
        // Sync the directory before deleting old packs. If a crash restores the old
        // `index.redb` directory entry, that catalog still needs its packs.
        PackWriter::sync_directory(&self.root)?;
        if self.reconciliation_needed() {
            self.reconcile_files()?;
        }
        if let Err(error) = self.remove_replacement() {
            tracing::warn!(%error, path = %self.root.display(), "index cleanup deferred");
        }
        self.healthy.store(true, Ordering::Release);
        self.compaction_due.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn open_pack(&self, id: &PackId) -> Result<File> {
        let path = PackWriter::path(&self.packs, id);
        File::open(&path).map_err(|source| {
            let error = if source.kind() == io::ErrorKind::NotFound {
                Error::invalid_source(source)
            } else {
                Error::from(source)
            };
            error.at(path)
        })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn compression(&self) -> Compression {
        self.options.compression
    }

    pub fn set_compression(&mut self, compression: Compression) {
        self.options.compression = compression;
    }

    /// Open a consistent read view.
    pub fn snapshot(&self) -> Result<Read<'_>> {
        Read::new(self).map_err(|error| error.at(self.root.clone()))
    }

    /// Begin one checkpoint. Drop all read views first so the writer has exclusive access.
    pub fn begin_write(&self) -> Result<Write<'_>> {
        Write::new(self).map_err(|error| error.at(self.root.clone()))
    }

    pub(crate) fn index(&self) -> Result<&redb::Database> {
        if !self.healthy.load(Ordering::Acquire) {
            return Err(Error::unavailable("storage checkpoint needs recovery").require_reopen());
        }
        Ok(self.index.as_ref().expect("healthy index"))
    }

    /// Clear records and values while preserving the saved identity table.
    /// This requires a recoverable identity table.
    pub fn invalidate(&mut self) -> Result<()> {
        self.reset(None)
            .map_err(|error| error.require_reopen().at(&self.root))
    }

    /// Discard query data and replace the saved identity table. Pass an empty slice to clear
    /// all assignments. If the index is corrupt, replace it while keeping the directory lock.
    pub fn rebuild(&mut self, identities: &[Identity<'_>]) -> Result<()> {
        self.reset(Some(identities))
            .map_err(|error| error.require_reopen().at(&self.root))
    }

    fn reset(&mut self, identities: Option<&[Identity<'_>]>) -> Result<()> {
        // `None` means keep the saved identity table, so the index must be readable.
        // With `Some`, we have the assignments needed to build a fresh index if necessary.
        match (self.clear_index(identities), identities) {
            (Ok(()), _) => {}
            (Err(error), Some(identities)) if error.kind() == ErrorKind::InvalidCache => {
                self.replace_index(identities)?;
            }
            (Err(error), _) => return Err(error),
        }
        self.finish_open()
    }

    fn clear_index(&mut self, identities: Option<&[Identity<'_>]>) -> Result<()> {
        // On Unix, a cleanup tool can replace `index.redb` with an empty file while Ajika
        // still has the original open. A rebuild through that handle can report success
        // while saving IDs to the old file. On restart, Ajika would open the empty index.
        // Reopen `index.redb` under the directory lock so the reset targets the file a
        // restart will read. Loading the index is enough because clearing the catalog
        // drops every pack reference, even when the packs are damaged.
        self.load_index(false)?;
        let index = self.index.as_ref().expect("loaded index");
        let write = index.begin_write()?;
        Self::clear_catalog(&write, identities)?;
        write.commit()?;
        Ok(())
    }

    fn clear_catalog(
        write: &redb::WriteTransaction,
        identities: Option<&[Identity<'_>]>,
    ) -> Result<()> {
        write.delete_table(RECORDS)?;
        write.delete_table(VALUES)?;
        write.delete_table(PACKS)?;
        drop(write.open_table(RECORDS)?);
        drop(write.open_table(VALUES)?);
        drop(write.open_table(PACKS)?);
        if let Some(identities) = identities {
            write.delete_table(IDENTITIES)?;
            let mut table = write.open_table(IDENTITIES)?;
            for identity in identities {
                table.insert((identity.namespace, identity.id), identity.key)?;
            }
        } else {
            drop(write.open_table(IDENTITIES)?);
        }
        Ok(())
    }

    fn replace_index(&mut self, identities: &[Identity<'_>]) -> Result<()> {
        self.index.take();
        self.files.get_mut().expect("pack handles").clear();
        let replacement = self.root.join(REBUILD_FILE);
        // Build the new catalog at a temporary path. If recovery stops before the rename,
        // the next opener can still inspect the published index and retry the repair.
        self.remove_replacement()?;
        let index = redb::Database::builder()
            .set_cache_size(INDEX_CACHE_BYTES)
            .create(&replacement)?;
        self.initialize_index(&index, identities)?;
        drop(index);
        fs::rename(replacement, self.root.join(INDEX_FILE))?;
        self.load_index(false)
    }

    fn remove_replacement(&self) -> Result<()> {
        match fs::remove_file(self.root.join(REBUILD_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Finish pack reclamation and shrink the index. This explicit idle operation is unbounded;
    /// ordinary query operations use the small passes in [`Self::maintain`] instead.
    /// Orphan deletion failures are logged and retried; those files remain charged to the budget.
    pub fn compact(&mut self) -> Result<bool> {
        let mut changed = false;
        while self.compact_step(true)? {
            changed = true;
        }
        self.reconcile_files()?;
        changed |= self.index.as_mut().expect("healthy index").compact()?;
        self.refresh_index_usage()?;
        self.compaction_due.store(false, Ordering::Relaxed);
        Ok(changed)
    }

    pub(crate) fn write_budget(&self) -> u64 {
        // Leave space for a maintenance pass to copy live values before removing old files.
        let headroom = (self.options.max_physical_bytes / 8).min(super::maintenance::PASS_BYTES);
        self.options
            .max_physical_bytes
            .saturating_sub(headroom)
            .saturating_sub(self.physical_bytes())
    }

    pub(crate) fn physical_bytes(&self) -> u64 {
        self.disk_usage
            .lock()
            .expect("disk usage")
            .map_or(u64::MAX, |usage| {
                usage.index_bytes.saturating_add(usage.pack_bytes)
            })
    }

    /// Normal commits only need to measure the new packs and retire packs made empty by that
    /// transaction. Existing live files are immutable, so their allocation remains unchanged.
    pub(crate) fn finish_file_changes(
        &self,
        created: &[PathBuf],
        retired: &[PackId],
    ) -> Result<()> {
        // Remove the known totals while files change. An early error then leaves no stale
        // budget installed; the caller requests reconciliation before another writer starts.
        let mut usage = self
            .disk_usage
            .lock()
            .expect("disk usage")
            .take()
            .expect("reconciled disk usage");
        for path in created {
            let metadata = fs::metadata(path).map_err(|error| Error::from(error).at(path))?;
            usage.pack_bytes = usage
                .pack_bytes
                .checked_add(Self::allocated(&metadata))
                .ok_or_else(|| Error::unavailable("storage size needs reconciliation"))?;
        }

        // A pack can be created and emptied in the same write. Count new files before
        // subtracting retired ones so that such a pack has no net charge.
        if !retired.is_empty() {
            self.files.lock().expect("pack handles").clear();
        }
        for id in retired {
            let path = PackWriter::path(&self.packs, id);
            let metadata = fs::metadata(&path).map_err(|error| Error::from(error).at(&path))?;
            fs::remove_file(&path).map_err(|error| Error::from(error).at(&path))?;
            usage.pack_bytes = usage
                .pack_bytes
                .checked_sub(Self::allocated(&metadata))
                .ok_or_else(|| Error::unavailable("storage size needs reconciliation"))?;
        }
        usage.index_bytes = Self::allocated(&fs::metadata(self.root.join(INDEX_FILE))?);
        *self.disk_usage.lock().expect("disk usage") = Some(usage);
        Ok(())
    }

    /// Rolling back a transaction or compacting the index can change its physical allocation
    /// without changing any pack files. Refresh just that part of the budget.
    pub(crate) fn refresh_index_usage(&self) -> Result<()> {
        let metadata = fs::metadata(self.root.join(INDEX_FILE))?;
        let mut usage = self.disk_usage.lock().expect("disk usage");
        usage.as_mut().expect("reconciled disk usage").index_bytes = Self::allocated(&metadata);
        Ok(())
    }

    /// File changes or an unknown budget require reconciliation. Ordinary compaction
    /// has its own scheduling flag and does not require discovering or measuring every file.
    pub(crate) fn reconciliation_needed(&self) -> bool {
        self.reconciliation_due.load(Ordering::Relaxed)
            || self.disk_usage.lock().expect("disk usage").is_none()
    }

    /// Rebuild usage from the published catalog and actual files, under exclusive access.
    /// An undeleted orphan stays counted and scheduled for retry. Errors mean that the catalog
    /// or the remaining files could not be read well enough to establish a trustworthy budget.
    pub(crate) fn reconcile_files(&self) -> Result<()> {
        *self.disk_usage.lock().expect("disk usage") = None;
        self.reconciliation_due.store(true, Ordering::Relaxed);
        self.files.lock().expect("pack handles").clear();
        let read = self.index.as_ref().expect("published index").begin_read()?;
        let mut keep = HashSet::new();
        for entry in read.open_table(PACKS)?.iter()? {
            let (id, _) = entry?;
            keep.insert(PackWriter::path(&self.packs, id.value()));
        }

        // Try deleting each orphan, then measure the files that remain. A deletion failure is
        // deferred locally; it does not make the size of a readable file unknown.
        let mut cleanup_deferred = false;
        let mut pack_bytes = 0;
        for entry in fs::read_dir(&self.packs)? {
            let entry = entry?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "pack")
                && !keep.contains(&path)
            {
                match fs::remove_file(&path) {
                    Ok(()) => continue,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        cleanup_deferred = true;
                        tracing::warn!(%error, path = %path.display(), "pack cleanup deferred");
                    }
                }
            }
            pack_bytes += Self::allocated(&entry.metadata()?);
        }
        let index_bytes = Self::allocated(&fs::metadata(self.root.join(INDEX_FILE))?);
        *self.disk_usage.lock().expect("disk usage") = Some(DiskUsage {
            index_bytes,
            pack_bytes,
        });
        self.reconciliation_due
            .store(cleanup_deferred, Ordering::Relaxed);
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

    /// Inspect contents without decoding payloads. This scans storage metadata, so use it for
    /// diagnostics.
    pub fn statistics(&self) -> Result<Statistics> {
        let read = self.snapshot()?;
        Statistics::read_index(&read.transaction, &self.root)
    }
}
