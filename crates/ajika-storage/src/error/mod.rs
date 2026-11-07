//! Error classification for the storage layer.
//!
//! Readers and writers keep the original I/O or decoding error for diagnostics. The category
//! lets callers decide whether to retry, reclaim space, or discard unreadable backing.

use std::{
    error::Error as StdError,
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Return early when a required condition fails. The error expression runs only on failure,
/// so callers can format details inline.
macro_rules! ensure {
    ($condition:expr, $error:expr $(,)?) => {
        if !($condition) {
            return Err($error);
        }
    };
}

pub(crate) use ensure;

pub type Result<T> = std::result::Result<T, Error>;
type BoxError = Box<dyn StdError + Send + Sync>;

/// What went wrong at the storage boundary. Check [`Error::requires_reopen`] separately
/// to see whether the failed operation made the owner unusable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Another owner has this cache open. Wait for it to close or use another cache directory.
    InUse,
    /// A write exceeded the byte budget. Reads and reclamation can still proceed.
    Capacity,
    /// Saved data is missing, invalid, or incompatible. The caller can discard the cache.
    InvalidCache,
    /// A read or write conflicts with active access. Finish the active scope before retrying.
    AccessConflict,
    /// An I/O operation failed, for example because file access was denied.
    Io,
    /// An earlier failure stopped the owner or the current write from accepting work.
    Unavailable,
    /// A storage failure outside the other categories, such as a compression failure.
    Unexpected,
}

#[derive(Debug)]
struct Details {
    kind: ErrorKind,
    cause: Cause,
    path: Option<PathBuf>,
    reopen: bool,
}

#[derive(Debug, thiserror::Error)]
enum Cause {
    #[error("{0}")]
    Message(String),
    #[error("{message}")]
    Source {
        message: &'static str,
        #[source]
        source: BoxError,
    },
    #[error("{operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
}

macro_rules! index_error {
    ($($kind:ty),* $(,)?) => {$(
        impl From<$kind> for Error {
            fn from(source: $kind) -> Self { Self::from(redb::Error::from(source)) }
        }
    )*};
}

/// A storage failure with its original cause and optional filesystem context.
#[derive(Clone, Debug)]
pub struct Error {
    inner: Arc<Details>,
}

impl Error {
    fn new(kind: ErrorKind, cause: Cause) -> Self {
        Self {
            inner: Arc::new(Details {
                kind,
                cause,
                path: None,
                reopen: false,
            }),
        }
    }

    pub fn kind(&self) -> ErrorKind {
        self.inner.kind
    }

    /// A path attached for diagnostics, such as the directory of a failed open.
    pub fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    /// Whether the owner must be reopened before it can serve reads or writes again.
    /// A failed append can affect just the current write. A failed index commit can also leave
    /// the published checkpoint uncertain, so reopening must resolve it first.
    pub fn requires_reopen(&self) -> bool {
        self.inner.reopen
    }

    /// Inspect the immediate I/O cause when one is available. For example, a missing pack can
    /// report [`ErrorKind::InvalidCache`] and still carry [`io::ErrorKind::NotFound`].
    pub fn io_error(&self) -> Option<&io::Error> {
        self.source()?.downcast_ref()
    }

    pub(crate) fn in_use() -> Self {
        Self::new(
            ErrorKind::InUse,
            Cause::Message("query storage is already open".into()),
        )
    }

    pub(crate) fn capacity() -> Self {
        Self::new(
            ErrorKind::Capacity,
            Cause::Message("query storage is full; compact it or discard cached results".into()),
        )
    }

    pub(crate) fn invalid_cache(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidCache, Cause::Message(message.into()))
    }

    pub(crate) fn access_conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::AccessConflict, Cause::Message(message.into()))
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unavailable, Cause::Message(message.into()))
    }

    pub(crate) fn unexpected(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::new(
            ErrorKind::Unexpected,
            Cause::Source {
                message: "storage operation failed",
                source: Box::new(source),
            },
        )
    }

    pub(crate) fn invalid_source(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::new(
            ErrorKind::InvalidCache,
            Cause::Source {
                message: "invalid cache contents",
                source: Box::new(source),
            },
        )
    }

    pub(crate) fn at(mut self, path: impl Into<PathBuf>) -> Self {
        // Errors are shared only after publication to callers; constructing filesystem context
        // does not clone an underlying OS error or discard its raw error code.
        let details = Arc::get_mut(&mut self.inner).expect("unshared error context");
        // A pack read can name the affected file even when its caller only knows the cache path.
        if details.path.is_none() {
            details.path = Some(path.into());
        }
        self
    }

    pub(crate) fn require_reopen(mut self) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("unshared error state")
            .reopen = true;
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.inner.cause)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner.cause.source()
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        // Storage readers know their expected lengths. An incomplete frame is invalid backing,
        // while permission and device failures still need their ordinary I/O recovery policy.
        let kind = if source.kind() == io::ErrorKind::UnexpectedEof {
            ErrorKind::InvalidCache
        } else {
            ErrorKind::Io
        };
        Self::new(
            kind,
            Cause::Io {
                operation: "storage I/O failed",
                source,
            },
        )
    }
}

impl From<redb::DatabaseError> for Error {
    fn from(source: redb::DatabaseError) -> Self {
        match source {
            // redb reports an empty index or a bad file signature as InvalidData during open.
            // In either case recovery can rebuild the index from caller-supplied assignments.
            redb::DatabaseError::Storage(redb::StorageError::Io(source))
                if source.kind() == io::ErrorKind::InvalidData =>
            {
                Self::invalid_source(source)
            }
            source => redb::Error::from(source).into(),
        }
    }
}

impl From<redb::Error> for Error {
    fn from(source: redb::Error) -> Self {
        // redb's operation-specific wrappers flatten into this enum. Normalize here rather
        // than exposing a different public variant for every index API.
        match source {
            redb::Error::DatabaseAlreadyOpen => Self::in_use(),
            // An index I/O error can latch the redb handle, so recovery reloads the checkpoint.
            redb::Error::Io(source) => Self::from(source).require_reopen(),
            source @ (redb::Error::Corrupted(_)
            | redb::Error::UpgradeRequired(_)
            | redb::Error::TableTypeMismatch { .. }
            | redb::Error::TypeDefinitionChanged { .. }
            | redb::Error::TableDoesNotExist(_)) => Self::invalid_source(source),
            source @ (redb::Error::PreviousIo
            | redb::Error::DatabaseClosed
            | redb::Error::LockPoisoned(_)) => Self::new(
                ErrorKind::Unavailable,
                Cause::Source {
                    message: "storage operation failed",
                    source: Box::new(source),
                },
            )
            .require_reopen(),
            source @ (redb::Error::TransactionInProgress
            | redb::Error::ReadTransactionStillInUse(_)
            | redb::Error::TableAlreadyOpen(_, _)) => Self::new(
                ErrorKind::AccessConflict,
                Cause::Source {
                    message: "storage operation failed",
                    source: Box::new(source),
                },
            ),
            source => Self::unexpected(source),
        }
    }
}

index_error!(
    redb::StorageError,
    redb::TableError,
    redb::TransactionError,
    redb::CommitError,
    redb::CompactionError
);

impl From<getrandom::Error> for Error {
    fn from(source: getrandom::Error) -> Self {
        Self::unexpected(source)
    }
}

impl From<lz4_flex::frame::Error> for Error {
    fn from(source: lz4_flex::frame::Error) -> Self {
        Self::unexpected(source)
    }
}
