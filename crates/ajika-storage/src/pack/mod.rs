//! Append values to private files, then seal them before publishing their locations.
//!
//! A unique file name is safe to create before the checkpoint commits: readers only use names
//! in the index. Failed preparation leaves harmless orphans. Once commit has been attempted,
//! only the recovered index can decide which files are safe to delete.

use std::{
    fs::{self, File},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{
    EncodedValue, Error, Id, Result,
    error::ensure,
    format::{self, FrameHeader, InstanceId, Location, PackHeader, PackId},
};

const TARGET_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) struct PackWriter {
    directory: PathBuf,
    instance: InstanceId,
    current: Option<OpenPack>,
    // Includes files whose initialization failed, so abort can remove them too.
    created: Vec<PathBuf>,
    written: u64,
    remaining: u64,
    commit_attempted: bool,
    failed: bool,
    sync_time: Duration,
}

impl PackWriter {
    pub(crate) fn new(directory: PathBuf, instance: InstanceId, remaining: u64) -> Self {
        Self {
            directory,
            instance,
            current: None,
            created: Vec::new(),
            written: 0,
            remaining,
            commit_attempted: false,
            failed: false,
            sync_time: Duration::ZERO,
        }
    }

    /// Append one independent frame. A large value can exceed the pack target; the target only
    /// controls rotation, and must not become an arbitrary limit on a caller's value type.
    pub(crate) fn append(&mut self, id: Id, value: EncodedValue) -> Result<Location> {
        let header = FrameHeader {
            id,
            raw_length: value.raw_length,
            stored_length: value.bytes.len() as u64,
            compression: value.compression,
            checksum: value.checksum,
        };
        let location = self.begin_frame(header)?;
        self.current
            .as_mut()
            .expect("pack created")
            .writer
            .write_all(&value.bytes)?;
        self.failed = false;
        Ok(location)
    }

    /// Relocation preserves the codec and checksum. Copy through a small buffer so maintenance
    /// never needs the decoded query, or even a complete allocation for its compressed bytes.
    pub(crate) fn copy_frame(
        &mut self,
        header: FrameHeader,
        source: &mut impl Read,
    ) -> Result<Location> {
        let expected = header.checksum;
        let mut remaining = header.stored_length;
        let location = self.begin_frame(header)?;
        let writer = &mut self.current.as_mut().expect("pack created").writer;
        let mut buffer = [0_u8; 64 * 1024];
        let mut hash = blake3::Hasher::new();
        while remaining > 0 {
            let length = remaining.min(buffer.len() as u64) as usize;
            source.read_exact(&mut buffer[..length])?;
            hash.update(&buffer[..length]);
            writer.write_all(&buffer[..length])?;
            remaining -= length as u64;
        }
        ensure!(
            hash.finalize().as_bytes() == &expected,
            Error::invalid_cache("invalid stored value checksum"),
        );
        self.failed = false;
        Ok(location)
    }

    pub(crate) fn account_inline(&mut self, length: u64) -> Result<()> {
        self.reserve(length.max(1))
    }

    fn reserve(&mut self, length: u64) -> Result<()> {
        if length > self.remaining.saturating_sub(self.written) {
            return Err(Error::capacity());
        }
        self.written += length;
        Ok(())
    }

    /// Reserve a complete frame before its bytes are written. A failed append may leave a
    /// partial prefix in the file; finish must then reject publication even if its caller
    /// caught the error and tried to commit the surrounding transaction.
    fn begin_frame(&mut self, header: FrameHeader) -> Result<Location> {
        if self.failed {
            return Err(Error::unavailable(
                "discard storage write after its failed append",
            ));
        }
        self.failed = true;
        self.reserve(header.frame_length())?;
        if self
            .current
            .as_ref()
            .is_some_and(|pack| pack.length >= TARGET_BYTES)
        {
            self.seal()?;
        }
        if self.current.is_none() {
            self.reserve(PackHeader::LENGTH)?;
            let id = format::new_id()?;
            let path = Self::path(&self.directory, &id);
            let file = File::create_new(&path)?;
            self.created.push(path);
            let mut writer = BufWriter::with_capacity(64 * 1024, file);
            writer.write_all(&format::encode(&PackHeader {
                magic: PackHeader::MAGIC,
                instance: self.instance,
                pack: id,
            })?)?;
            self.current = Some(OpenPack {
                id,
                writer,
                length: PackHeader::LENGTH,
            });
        }
        let pack = self.current.as_mut().expect("pack created");
        let offset = pack.length;
        pack.writer.write_all(&format::encode(&header)?)?;
        pack.length += header.frame_length();
        Ok(Location::Pack {
            pack: pack.id,
            offset,
            header,
        })
    }

    fn seal(&mut self) -> Result<()> {
        if let Some(mut pack) = self.current.take() {
            pack.writer.flush()?;
            let started = Instant::now();
            pack.writer.get_ref().sync_all()?;
            self.sync_time += started.elapsed();
        }
        Ok(())
    }

    /// Complete the bytes and file names before the index commit can name them.
    pub(crate) fn finish(&mut self) -> Result<Duration> {
        if self.failed {
            return Err(Error::unavailable(
                "discard storage write after its failed append",
            ));
        }
        self.seal()?;
        if !self.created.is_empty() {
            let started = Instant::now();
            Self::sync_directory(&self.directory)?;
            self.sync_time += started.elapsed();
        }
        Ok(self.sync_time)
    }

    pub(crate) fn commit_attempted(&mut self) {
        self.commit_attempted = true;
    }

    pub(crate) fn path(directory: &Path, id: &PackId) -> PathBuf {
        let name = id
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        directory.join(format!("{name}.pack"))
    }

    pub(crate) fn sync_directory(directory: &Path) -> Result<()> {
        // Unix lets us synchronize new file names explicitly. Windows lacks the same portable
        // directory operation; a missing pack after power loss is rejected on reopen there.
        // TODO: validate a stronger Windows name-durability protocol before promising it there.
        #[cfg(unix)]
        File::open(directory)?.sync_all()?;
        #[cfg(not(unix))]
        let _ = directory;
        Ok(())
    }
}

impl Drop for PackWriter {
    fn drop(&mut self) {
        // Close handles before unlinking, including on Windows. A commit error may mean the
        // index was published, so leave those files for catalog-based cleanup on reopen.
        self.current.take();
        if !self.commit_attempted {
            for path in &self.created {
                if let Err(error) = fs::remove_file(path) {
                    tracing::warn!(%error, path = %path.display(), "could not remove abandoned pack");
                }
            }
        }
    }
}

struct OpenPack {
    id: PackId,
    writer: BufWriter<File>,
    length: u64,
}
