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
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use redb::{ReadableDatabase, ReadableTable};

use crate::{
    Compression, Error, ErrorKind, Read, Result, Statistics, Write,
    error::ensure,
    format::{self, HEADER, IDENTITIES, PACKS, PackHeader, PackId, PackInfo, RECORDS, VALUES},
    pack::PackWriter,
};

const INDEX_FILE: &str = "index.redb";
const INDEX_CACHE_BYTES: usize = 8 * 1024 * 1024;
const REBUILD_FILE: &str = "index.rebuild.redb";

/// An interned key and its stable ID within a namespace.
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
    index: Option<redb::Database>,
    epoch: String,
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
                instance: [0; 16],
                access: RwLock::new(()),
                healthy: AtomicBool::new(false),
                physical_bytes: AtomicU64::new(0),
                maintenance_due: AtomicBool::new(true),
                options,
                files: Mutex::default(),
                _owner: owner,
            };

            if let Err(error) = database.load(true) {
                if error.kind() != ErrorKind::InvalidCache {
                    return Err(error);
                }
                // A missing or damaged pack need not cost the caller its saved identities.
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

    /// Reload the disk checkpoint while retaining ownership of the directory. A failed reload
    /// leaves reads and writes unavailable until a reload or rebuild succeeds.
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
        self.files.get_mut().expect("pack handles").clear();
        self.index.take();

        let path = self.root.join(INDEX_FILE);
        let mut builder = redb::Database::builder();
        builder.set_cache_size(INDEX_CACHE_BYTES);
        let index = if create {
            builder.create(&path)?
        } else {
            builder.open(&path).map_err(|error| match error {
                redb::DatabaseError::Storage(redb::StorageError::Io(source))
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    Error::invalid_source(source)
                }
                error => error.into(),
            })?
        };
        self.instance = self.initialize_index(&index, create.then_some(&[]))?;
        self.index = Some(index);
        Ok(())
    }

    fn initialize_index(
        &self,
        index: &redb::Database,
        initialize: Option<&[Identity<'_>]>,
    ) -> Result<PackId> {
        let write = index.begin_write()?;
        let (compatible, instance) = {
            let mut header = write.open_table(HEADER)?;
            let compatible = header
                .get("epoch")?
                .is_some_and(|value| value.value() == self.epoch.as_bytes());
            let instance = if compatible {
                header
                    .get("instance")?
                    .ok_or_else(|| Error::invalid_cache("missing storage instance"))?
                    .value()
                    .try_into()
                    .map_err(Error::invalid_source)?
            } else if initialize.is_some() {
                let instance = format::new_id()?;
                header.insert("epoch", self.epoch.as_bytes())?;
                header.insert("instance", instance.as_slice())?;
                instance
            } else {
                return Err(Error::invalid_cache("storage epoch changed"));
            };
            (compatible, instance)
        };
        if !compatible {
            Self::clear_catalog(&write, initialize)?;
        }
        write.commit()?;
        Ok(instance)
    }

    fn validate_catalog(&self) -> Result<()> {
        let index = self.index.as_ref().expect("loaded index");
        let read = index.begin_read()?;
        drop(read.open_table(IDENTITIES)?);
        drop(read.open_table(RECORDS)?);
        drop(read.open_table(VALUES)?);
        // Opening checks pack existence and framing. Reads verify payload checksums on demand.
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
        // A replacement index must be durable before retiring files the old catalog could name.
        PackWriter::sync_directory(&self.root)?;
        if let Err(error) = self.clean_orphans() {
            if error.requires_reopen() || error.kind() == ErrorKind::InvalidCache {
                return Err(error);
            }
            tracing::warn!(
                %error,
                cause = error.source().map(tracing::field::display),
                path = %self.root.display(),
                "pack cleanup deferred"
            );
        }
        if let Err(error) = self.remove_replacement() {
            tracing::warn!(%error, path = %self.root.display(), "index cleanup deferred");
        }
        self.refresh_physical_bytes()?;
        self.healthy.store(true, Ordering::Release);
        self.maintenance_due.store(true, Ordering::Relaxed);
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

    /// Clear records and values together. If `reset_identities` is true, this also clears saved
    /// identity assignments and invalidates any IDs that refer to them. Preserving identities
    /// requires a recoverable identity table.
    pub fn invalidate(&mut self, reset_identities: bool) -> Result<()> {
        self.reset(reset_identities.then_some(&[]))
            .map_err(|error| error.require_reopen().at(&self.root))
    }

    /// Discard query data and publish the caller's authoritative identity assignments together.
    /// If the existing index is corrupt, replace it while keeping the directory lock.
    pub fn rebuild(&mut self, identities: &[Identity<'_>]) -> Result<()> {
        self.reset(Some(identities))
            .map_err(|error| error.require_reopen().at(&self.root))
    }

    fn reset(&mut self, identities: Option<&[Identity<'_>]>) -> Result<()> {
        // A reset needs the index, but it can discard pack references without reading them.
        let clear = if self.healthy.load(Ordering::Acquire) {
            match self.clear_index(identities) {
                Err(error)
                    if error.requires_reopen() && error.kind() != ErrorKind::InvalidCache =>
                {
                    self.load_index(false)
                        .and_then(|()| self.clear_index(identities))
                }
                result => result,
            }
        } else {
            self.load_index(false)
                .and_then(|()| self.clear_index(identities))
        };
        match (clear, identities) {
            (Ok(()), _) => {}
            (Err(error), Some(identities)) if error.kind() == ErrorKind::InvalidCache => {
                self.replace_index(identities)?;
            }
            (Err(error), _) => return Err(error),
        }
        self.finish_open()
    }

    fn clear_index(&self, identities: Option<&[Identity<'_>]>) -> Result<()> {
        self.healthy.store(false, Ordering::Release);
        ensure!(
            self.root.join(INDEX_FILE).try_exists()?,
            Error::invalid_cache("missing index file"),
        );
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
        // A restart can still use the published index if preparation was interrupted.
        self.remove_replacement()?;
        let index = redb::Database::builder()
            .set_cache_size(INDEX_CACHE_BYTES)
            .create(&replacement)?;
        self.initialize_index(&index, Some(identities))?;
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
    pub fn compact(&mut self) -> Result<bool> {
        let mut changed = false;
        while self.compact_step(true)? {
            changed = true;
        }
        self.files.get_mut().expect("pack handles").clear();
        self.clean_orphans()?;
        changed |= self.index.as_mut().expect("healthy index").compact()?;
        self.refresh_physical_bytes()?;
        Ok(changed)
    }

    pub(crate) fn write_budget(&self) -> u64 {
        // Leave space for a maintenance pass to copy live values before removing old files.
        let headroom = (self.options.max_physical_bytes / 8).min(super::maintenance::PASS_BYTES);
        self.options
            .max_physical_bytes
            .saturating_sub(headroom)
            .saturating_sub(self.physical_bytes.load(Ordering::Relaxed))
    }

    /// Remove files that neither a successful checkpoint nor its live values can reach.
    /// This is used only while opening, or under exclusive access after publication.
    pub(crate) fn clean_orphans(&self) -> Result<()> {
        let read = self.index.as_ref().expect("published index").begin_read()?;
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
        let mut bytes = Self::allocated(&fs::metadata(self.root.join(INDEX_FILE))?);
        for entry in fs::read_dir(&self.packs)? {
            bytes += Self::allocated(&entry?.metadata()?);
        }
        self.physical_bytes.store(bytes, Ordering::Relaxed);
        Ok(())
    }

    /// Inspect contents without decoding payloads. This scans storage metadata, so use it for
    /// diagnostics.
    pub fn statistics(&self) -> Result<Statistics> {
        let read = self.snapshot()?;
        Statistics::read_index(&read.transaction, &self.root)
    }
}
