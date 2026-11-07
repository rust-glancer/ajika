//! Stable recovery categories, independent of the index and compression implementations.

use std::{
    error::Error as StdError,
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Return the supplied error when a required condition fails.
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

/// The decisions a storage caller can make without inspecting dependency-specific errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    InUse,
    Capacity,
    InvalidCache,
    AccessConflict,
    Io,
    Unavailable,
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
/// Classification describes the cause; [`requires_reopen`](Self::requires_reopen) separately
/// records whether a failed publication made the owner's state uncertain.
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

    pub fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    pub fn requires_reopen(&self) -> bool {
        self.inner.reopen
    }

    /// The I/O error remains directly accessible even when it originated inside redb or decoding.
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
        Arc::get_mut(&mut self.inner)
            .expect("unshared error context")
            .path = Some(path.into());
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

impl From<redb::Error> for Error {
    fn from(source: redb::Error) -> Self {
        // redb's operation-specific wrappers flatten into this enum. Normalize here rather
        // than exposing a different public variant for every index API.
        match source {
            redb::Error::DatabaseAlreadyOpen => Self::in_use(),
            redb::Error::Io(source) => source.into(),
            source @ (redb::Error::Corrupted(_)
            | redb::Error::UpgradeRequired(_)
            | redb::Error::TableTypeMismatch { .. }
            | redb::Error::TypeDefinitionChanged { .. }
            | redb::Error::TableDoesNotExist(_)) => Self::invalid_source(source),
            source @ (redb::Error::PreviousIo
            | redb::Error::DatabaseClosed
            | redb::Error::TransactionPoisoned
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
    redb::DatabaseError,
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
