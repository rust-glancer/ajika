use std::{thread, time::Duration};

use crate::{Compression, Retention};

/// Retention and execution settings for a database. Disk databases write checkpoints
/// at the end of each run by default.
#[derive(Debug, Clone)]
#[must_use]
pub struct DatabaseOptions {
    pub(crate) retention: Retention,
    pub(crate) session_retention: Retention,
    pub(crate) parallelism: usize,
    pub(crate) max_pending_bytes: Option<u64>,
    pub(crate) storage: ajika_storage::Options,
}

impl DatabaseOptions {
    pub fn new(retention: Retention) -> Self {
        Self {
            retention,
            session_retention: Retention {
                max_entries: 16_384,
                max_bytes: 64 * 1024 * 1024,
                max_idle: Duration::from_secs(300),
            },
            parallelism: thread::available_parallelism().map_or(1, usize::from),
            max_pending_bytes: None,
            storage: ajika_storage::Options::default(),
        }
    }

    /// Bound retained session work. Defaults to 64 MiB of estimated encoded data, 16,384
    /// producers, and five minutes idle, independently of the persistent retention limits.
    pub fn session_retention(mut self, retention: Retention) -> Self {
        self.session_retention = retention;
        self
    }

    /// Limit query and checkpoint workers, including the calling thread.
    /// Defaults to the available parallelism reported by the operating system.
    ///
    /// # Panics
    ///
    /// Panics if `workers` is zero.
    pub fn parallelism(mut self, workers: usize) -> Self {
        assert!(workers > 0, "parallelism must be positive");
        self.parallelism = workers;
        self
    }

    /// Choose compression for disk payloads. Defaults to LZ4; existing values keep their codec.
    pub fn compression(mut self, compression: Compression) -> Self {
        self.storage.compression = compression;
        self
    }

    /// Keep completed persistent work in memory for later runs to reuse. Call
    /// [`crate::Database::collect`] to write it at an idle boundary. Dropping the database
    /// discards work still in memory.
    ///
    /// When pending records and their resident payloads exceed `max_pending_bytes`, the run
    /// writes and synchronizes them before returning. If disk space is tight, it can evict
    /// records eligible for collection first. The budget uses estimated encoded sizes.
    /// Actual heap usage may differ, and a single operation can exceed the budget.
    pub fn defer_checkpoints(mut self, max_pending_bytes: u64) -> Self {
        self.max_pending_bytes = Some(max_pending_bytes);
        self
    }

    /// Set the soft disk budget, including the index and obsolete files. Defaults to 32 GiB.
    /// Removals and maintenance remain available when the budget is exhausted.
    pub fn max_physical_bytes(mut self, bytes: u64) -> Self {
        self.storage.max_physical_bytes = bytes;
        self
    }
}
