//! Reasons a query or database operation stopped.
//!
//! The request boundary handles cancellation when a newer request makes an answer obsolete.
//! For a failure, the error keeps both the category a caller can act on and the cause it can log.

use std::{error::Error as StdError, fmt, io, path::Path, sync::Arc};

use ajika_storage::Error as StorageError;

use crate::{PersistenceErrorKind, QueryCycle};

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

/// A request stopped at a cooperative check because its answer was no longer needed.
/// A cache write may already have committed when the request observes cancellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("operation cancelled at {checkpoint}")]
pub struct Cancelled {
    checkpoint: &'static str,
}

impl Cancelled {
    /// Adapt an external cancellation signal with a label such as `"request superseded"`.
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

#[derive(Debug, thiserror::Error)]
enum PersistenceRepr {
    #[error(transparent)]
    Storage(StorageError),
    #[error("invalid cached query: {message}")]
    InvalidCache {
        message: String,
        #[source]
        source: Option<BoxError>,
    },
    #[error("{0}")]
    Unavailable(String),
}

/// A failure accessing Ajika's saved work. A query record that cannot be decoded and
/// a damaged storage frame both report [`PersistenceErrorKind::InvalidCache`]. Use
/// [`Error::requires_reopen`] to decide whether the owner can still serve queries.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct PersistenceError(PersistenceRepr);

impl PersistenceError {
    pub fn kind(&self) -> PersistenceErrorKind {
        match &self.0 {
            PersistenceRepr::Storage(error) => error.kind(),
            PersistenceRepr::InvalidCache { .. } => PersistenceErrorKind::InvalidCache,
            PersistenceRepr::Unavailable(_) => PersistenceErrorKind::Unavailable,
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

/// The reason an operation stopped, with details for callers that can act on it.
/// For example, a cycle includes the calls to show in a diagnostic. A persistence failure can
/// distinguish a full cache from unreadable contents.
#[derive(Debug)]
#[non_exhaustive]
pub enum ErrorKind<'a> {
    /// Work stopped because the caller no longer needed its answer.
    Cancelled(&'a Cancelled),
    /// Active query calls formed a dependency cycle.
    Cycle(&'a QueryCycle),
    /// A required input, output, or interned identity could not be found.
    Unavailable(&'a UnavailableData),
    /// Ajika could not open, read, or update its saved work.
    Persistence(&'a PersistenceError),
    /// Application code failed, for example while reading an input file.
    Application(&'a (dyn StdError + Send + Sync + 'static)),
    /// A query rule was violated, such as emitting the same output slot twice.
    Contract,
    /// The runtime failed without a specific recovery action for the caller.
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

/// Why an Ajika operation stopped. Workers can share the same error so the caller gets its
/// original cause even when another worker was the first to see it.
/// Use [`Error::is_cancelled`] at the request boundary and [`Error::kind`] when a particular
/// failure needs recovery.
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

    /// Whether the operation stopped because the caller no longer needs the answer. Inside a
    /// request, propagate the error with `?` until the code that decides whether to publish a reply.
    ///
    /// A handler can skip the reply when a newer request supersedes it.
    ///
    /// ```
    /// fn reply<T>(result: ajika::Result<T>) -> ajika::Result<Option<T>> {
    ///     match result {
    ///         Ok(value) => Ok(Some(value)),
    ///         Err(error) if error.is_cancelled() => Ok(None),
    ///         Err(error) => Err(error),
    ///     }
    /// }
    /// ```
    pub fn is_cancelled(&self) -> bool {
        matches!(self.kind(), ErrorKind::Cancelled(_))
    }

    /// Attach a failure from application code. For example, use `.map_err(Error::application)`
    /// when acquiring an input from a file. Propagate Ajika errors with `?` to keep their category.
    ///
    /// Accepts concrete errors, boxed standard errors, and reports such as `anyhow::Error`.
    /// Pass the concrete error when callers need typed recovery. Boxing an `anyhow::Error`
    /// keeps its diagnostic chain and may erase downcasts to the original error type.
    pub fn application(source: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Self::new(ErrorRepr::Application(source.into()))
    }

    /// Report a violated query rule, for example when a validator finds an invalid arena index.
    pub fn contract(message: impl Into<String>) -> Self {
        Self::new(ErrorRepr::Contract {
            message: message.into(),
            source: None,
        })
    }

    /// Add a message about the failed step, such as `"loading analysis inputs"`. The original
    /// cause, category, and recovery status remain available. Cancellation keeps its checkpoint.
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

    /// Whether the database needs recovery before serving more queries. A failed checkpoint
    /// can leave the owner unusable even when a healthy read could return the same I/O cause.
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
        matches!(self.kind(), ErrorKind::Persistence(error) if error.kind() == PersistenceErrorKind::Capacity)
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

impl From<StorageError> for Error {
    fn from(source: StorageError) -> Self {
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

/// Apply [`Error::context`] while propagating a failed result.
pub trait ResultExt: Sized {
    fn context(self, message: impl Into<String>) -> Self;
}

impl<T> ResultExt for Result<T> {
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
            matches!(error.kind(), super::ErrorKind::Persistence(error) if error.kind() == super::PersistenceErrorKind::Io)
        );
    }
}
