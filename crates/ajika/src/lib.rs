#![doc = include_str!("../README.md")]

// For implementation work, start with input and the query contract in model, then Database::run
// in runtime. The evaluation path decides whether to reuse, load, or compute a query answer.
// The backend checkpoints completed work; ajika-storage handles bytes and atomic publication.
// Query code never executes while holding the storage writer.
mod codec;
mod error;
mod input;
mod interner;
mod model;
mod runtime;

#[doc(hidden)]
pub mod __private;

/// # Related APIs
///
/// The generated provider implements [`Query`]. Query keys and persistent result values use
/// [`Persist`], and the `validate` option implements [`Query::validate`].
#[doc(inline)]
pub use ajika_macros::query;
pub use ajika_macros::{input, interned, output, register};
pub use ajika_storage::{
    Compression, ErrorKind as PersistenceErrorKind, Statistics as StorageStatistics,
};

pub use self::{
    codec::Persist,
    error::{Cancelled, Error, ErrorKind, PersistenceError, Result, ResultExt, UnavailableData},
    input::{Field, Input, InputUpdate, Inputs},
    interner::Interner,
    model::{Durability, Lifetime, Query, Retention, Stamp, Statistics, Storage},
    runtime::{Cancellation, Database, Db, Output, OutputSlot, QueryCall, QueryCycle},
};
