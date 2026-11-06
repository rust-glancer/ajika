//! One result type carries expected cancellation and actionable failure categories.

use std::{error::Error as StdError, fmt, io, path::Path, sync::Arc};

use crate::{QueryCycle, storage};

pub type Result<T> = std::result::Result<T, Error>;

type BoxError = Box<dyn StdError + Send + Sync>;

/// The cooperative checkpoint at which an operation observed that its answer was obsolete.
/// It makes no promise that a completed cache checkpoint was rolled back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("operation cancelled at {checkpoint}")]
pub struct Cancelled {
    checkpoint: &'static str,
}

impl Cancelled {
    pub fn at(checkpoint: &'static str) -> Self {
        Self { checkpoint }
    }

    pub fn checkpoint(&self) -> &'static str {
        self.checkpoint
    }
}

/// Data the caller must supply again or reacquire under the current database inputs.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UnavailableData {
    #[error("input has not been supplied: {name}")]
    Input { name: &'static str },
    #[error("output is unavailable from {producer}")]
    Output { producer: String, slot: Vec<u8> },
    #[error("unknown identity {id} in {namespace}")]
    Identity { namespace: &'static str, id: u32 },
}

pub use storage::ErrorKind as PersistenceKind;

#[derive(Debug, thiserror::Error)]
enum PersistenceRepr {
    #[error(transparent)]
    Storage(storage::Error),
    #[error("invalid cached query: {message}")]
    InvalidCache {
        message: String,
        #[source]
        source: Option<BoxError>,
    },
    #[error("{0}")]
    Unavailable(String),
}

/// Persistence failures include saved query decoding as well as the byte storage layer.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct PersistenceError(PersistenceRepr);

impl PersistenceError {
    pub fn kind(&self) -> PersistenceKind {
        match &self.0 {
            PersistenceRepr::Storage(error) => error.kind(),
            PersistenceRepr::InvalidCache { .. } => PersistenceKind::InvalidCache,
            PersistenceRepr::Unavailable(_) => PersistenceKind::Unavailable,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        match &self.0 {
            PersistenceRepr::Storage(error) => error.path(),
            _ => None,
        }
    }

    pub fn io_error(&self) -> Option<&io::Error> {
        match &self.0 {
            PersistenceRepr::Storage(error) => error.io_error(),
            _ => None,
        }
    }

    fn requires_reopen(&self) -> bool {
        match &self.0 {
            PersistenceRepr::Storage(error) => error.requires_reopen(),
            PersistenceRepr::Unavailable(_) => true,
            _ => false,
        }
    }
}

/// A borrowed classification keeps useful payloads available without exposing implementation errors.
#[derive(Debug)]
#[non_exhaustive]
pub enum ErrorKind<'a> {
    Cancelled(&'a Cancelled),
    Cycle(&'a QueryCycle),
    Unavailable(&'a UnavailableData),
    Persistence(&'a PersistenceError),
    Application(&'a (dyn StdError + Send + Sync + 'static)),
    Contract,
    Unexpected,
}

#[derive(Debug, thiserror::Error)]
enum ErrorRepr {
    #[error(transparent)]
    Cancelled(Cancelled),
    #[error(transparent)]
    Cycle(QueryCycle),
    #[error(transparent)]
    Unavailable(UnavailableData),
    #[error(transparent)]
    Persistence(PersistenceError),
    #[error("application query failed")]
    Application(#[source] BoxError),
    #[error("query contract violated: {message}")]
    Contract {
        message: String,
        #[source]
        source: Option<BoxError>,
    },
    #[error("query runtime failed")]
    Unexpected(#[source] BoxError),
    #[error("{message}")]
    Context {
        message: String,
        #[source]
        source: Error,
    },
    #[error("query runtime failed: {0}")]
    Invariant(String),
}

/// An operation produced no answer. Inspect cancellation at the request boundary; other errors
/// retain their recovery category and original cause when shared across query workers.
#[derive(Clone, Debug)]
pub struct Error {
    inner: Arc<ErrorRepr>,
    reopen: bool,
}

impl Error {
    fn new(inner: ErrorRepr) -> Self {
        let reopen = matches!(&inner, ErrorRepr::Persistence(error) if error.requires_reopen());
        Self {
            inner: Arc::new(inner),
            reopen,
        }
    }

    pub fn kind(&self) -> ErrorKind<'_> {
        match self.inner.as_ref() {
            ErrorRepr::Cancelled(cancelled) => ErrorKind::Cancelled(cancelled),
            ErrorRepr::Cycle(cycle) => ErrorKind::Cycle(cycle),
            ErrorRepr::Unavailable(data) => ErrorKind::Unavailable(data),
            ErrorRepr::Persistence(error) => ErrorKind::Persistence(error),
            ErrorRepr::Application(source) => ErrorKind::Application(source.as_ref()),
            ErrorRepr::Contract { .. } => ErrorKind::Contract,
            ErrorRepr::Unexpected(_) => ErrorKind::Unexpected,
            ErrorRepr::Invariant(_) => ErrorKind::Unexpected,
            ErrorRepr::Context { source, .. } => source.kind(),
        }
    }

    /// Whether this is an expected stop because the caller no longer needs the answer.
    pub fn is_cancelled(&self) -> bool {
        matches!(self.kind(), ErrorKind::Cancelled(_))
    }

    /// The application extension point is explicit so an existing Ajika failure is never
    /// accidentally erased by a blanket conversion for all standard errors.
    ///
    /// Accepts concrete errors, boxed standard errors, and reports such as `anyhow::Error`.
    /// Concrete errors remain downcastable; a report's conversion to a boxed standard error
    /// may erase that ability. Pass the concrete error when callers need typed recovery.
    pub fn application(source: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Self::new(ErrorRepr::Application(source.into()))
    }

    pub fn contract(message: impl Into<String>) -> Self {
        Self::new(ErrorRepr::Contract {
            message: message.into(),
            source: None,
        })
    }

    pub fn context(self, message: impl Into<String>) -> Self {
        if self.is_cancelled() {
            return self;
        }
        let reopen = self.reopen;
        Self {
            inner: Arc::new(ErrorRepr::Context {
                message: message.into(),
                source: self,
            }),
            reopen,
        }
    }

    /// Whether this operation left the owner unusable. Recovery status belongs to this outer
    /// error, rather than to the underlying cause, which may also occur during a healthy read.
    pub fn requires_reopen(&self) -> bool {
        self.reopen
    }

    pub(crate) fn require_reopen(mut self) -> Self {
        self.reopen = true;
        self
    }

    pub(crate) fn invariant(message: impl Into<String>) -> Self {
        Self::new(ErrorRepr::Invariant(message.into()))
    }

    pub(crate) fn is_capacity(&self) -> bool {
        matches!(self.kind(), ErrorKind::Persistence(error) if error.kind() == PersistenceKind::Capacity)
    }

    pub(crate) fn invalid_cache(message: impl Into<String>) -> Self {
        Self::from(PersistenceError(PersistenceRepr::InvalidCache {
            message: message.into(),
            source: None,
        }))
    }

    pub(crate) fn cache_decode(source: wincode::ReadError) -> Self {
        Self::from(PersistenceError(PersistenceRepr::InvalidCache {
            message: "could not decode saved bytes".into(),
            source: Some(Box::new(source)),
        }))
    }

    pub(crate) fn value_encode(source: wincode::WriteError) -> Self {
        Self::new(ErrorRepr::Contract {
            message: "could not encode value".into(),
            source: Some(Box::new(source)),
        })
    }

    pub(crate) fn unavailable_storage(message: impl Into<String>) -> Self {
        Self::from(PersistenceError(PersistenceRepr::Unavailable(
            message.into(),
        )))
    }

    pub(crate) fn unexpected(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::new(ErrorRepr::Unexpected(Box::new(source)))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Applications choose how to render the standard source chain. Display only supplies
        // this error's summary, including when a surrounding reporter uses alternate formatting.
        write!(f, "{}", self.inner)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner.source()
    }
}

impl From<storage::Error> for Error {
    fn from(source: storage::Error) -> Self {
        Self::from(PersistenceError(PersistenceRepr::Storage(source)))
    }
}

impl From<PersistenceError> for Error {
    fn from(source: PersistenceError) -> Self {
        Self::new(ErrorRepr::Persistence(source))
    }
}

impl From<Cancelled> for Error {
    fn from(source: Cancelled) -> Self {
        Self::new(ErrorRepr::Cancelled(source))
    }
}

impl From<QueryCycle> for Error {
    fn from(source: QueryCycle) -> Self {
        Self::new(ErrorRepr::Cycle(source))
    }
}

impl From<UnavailableData> for Error {
    fn from(source: UnavailableData) -> Self {
        Self::new(ErrorRepr::Unavailable(source))
    }
}

impl From<std::num::TryFromIntError> for Error {
    fn from(source: std::num::TryFromIntError) -> Self {
        Self::unexpected(source)
    }
}

impl From<getrandom::Error> for Error {
    fn from(source: getrandom::Error) -> Self {
        Self::unexpected(source)
    }
}

/// Diagnostic context preserves the error's category and cancellation checkpoint.
pub trait ResultExt<T>: Sized {
    fn context(self, message: impl Into<String>) -> Self;
}

impl<T> ResultExt<T> for Result<T> {
    fn context(self, message: impl Into<String>) -> Self {
        self.map_err(|error| error.context(message))
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn standard_reporters_include_each_cause_once() {
        let error = Error::from(ajika_storage::Error::from(std::io::Error::other(
            "pack device unavailable",
        )));
        let cases = [
            (error.clone(), "storage I/O failed: pack device unavailable"),
            (
                error.clone().context("saving results"),
                "saving results: storage I/O failed: pack device unavailable",
            ),
            (
                error.require_reopen().context("saving results"),
                "saving results: storage I/O failed: pack device unavailable",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(format!("{:#}", anyhow::Error::new(error)), expected);
        }
    }

    #[test]
    fn recovery_status_survives_context_without_changing_the_cause() {
        let error = Error::from(ajika_storage::Error::from(std::io::Error::other(
            "pack write failed",
        )));
        let shared = error.clone();
        let error = error.require_reopen().context("saving results");
        assert!(error.requires_reopen());
        assert!(
            !shared.requires_reopen(),
            "recovery status describes the failed operation"
        );
        assert!(
            matches!(error.kind(), super::ErrorKind::Persistence(error) if error.kind() == super::PersistenceKind::Io)
        );
    }
}
