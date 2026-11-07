//! Store opaque records and independently readable values as one atomic checkpoint.
//!
//! Callers serialize values, optionally compress them with [`Compression::encode`], and publish
//! them through a [`Write`]. Dropping an uncommitted write preserves the previous checkpoint.
//! [`Read`] returns owned bytes that outlive the read view. Drop read views before writing.
//! Call [`Database::maintain`] between operations to reclaim disk space.
//!
//! The directory is a disposable cache, exclusively owned until [`Database`] drops.
//! Opening an unreadable cache can reset it. Errors during use are returned to the caller.

mod database;
mod error;
mod format;
mod maintenance;
mod pack;
mod statistics;
mod transaction;
mod value;

pub use self::{
    database::{Database, Options},
    error::{Error, ErrorKind, Result},
    statistics::Statistics,
    transaction::{Read, Write},
    value::{Compression, EncodedValue},
};

/// A caller-assigned value or record address. Replacing bytes preserves this identity.
pub type Id = [u8; 32];

#[cfg(test)]
mod tests;
