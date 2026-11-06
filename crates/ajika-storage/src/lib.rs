//! Saving opaque records and independently readable values as one checkpoint.
//!
//! Small records and value locations live in redb. Larger values live in immutable pack files:
//! each location names one file range, so loading a value does not decode its neighbors. A write
//! finishes and syncs its new packs before committing the index that makes them reachable.
//!
//! This crate knows nothing about query dependencies or Rust result types. Callers serialize
//! values, optionally compress them with [`Compression::encode`], and publish them through a
//! [`Write`]. Dropping an uncommitted write keeps the previous checkpoint. [`Read`] returns owned
//! bytes, and holds a read guard so maintenance cannot delete files underneath it.
//! Call [`Database::maintain`] between operations to reclaim obsolete frames and consolidate
//! small files. Its committed scan position lets work continue across operations and restarts.
//!
//! The directory is a disposable cache, exclusively owned until [`Database`] drops. Opening
//! unreadable backing resets the index and packs together. Runtime errors are returned to the
//! caller; they never start an automatic rebuild loop.

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
