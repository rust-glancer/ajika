//! The serialization contract shared by query declarations and persistent storage.
//!
//! [`Persist`] describes values Ajika can encode and reconstruct as owned Rust objects. The helpers
//! use the same wincode configuration for saved keys, dependency records, and result payloads.
//! Storage decides how those bytes are buffered or split into blocks; this module keeps their
//! encoding consistent and independent of the particular storage operation.

use anyhow::Result;
use wincode::{SchemaReadOwned, SchemaWrite};

// This is a local, versioned cache. A query must not acquire an arbitrary size limit merely
// because one of its fields is a large Vec. Corrupted sequence lengths can still exhaust memory;
// this codec is not intended for untrusted input.
type Config = wincode::config::Configuration<true, { usize::MAX }>;
const CONFIG: Config = wincode::config::Configuration::default().disable_preallocation_size_limit();

/// A value Ajika can serialize and later reconstruct without borrowing the encoded bytes.
/// Query keys, stored results, and internal records use this same bound. Owned decoding lets
/// storage release its temporary buffers and read transactions before returning the value.
///
/// Usually deriving [`wincode::SchemaRead`](trait@wincode::SchemaRead) and
/// [`wincode::SchemaWrite`](trait@wincode::SchemaWrite) is enough. The blanket impl below gives
/// those types [`Persist`] without adding a second serialization implementation.
pub trait Persist: SchemaWrite<Config, Src = Self> + SchemaReadOwned<Config, Dst = Self> {}

impl<T> Persist for T where T: SchemaWrite<Config, Src = Self> + SchemaReadOwned<Config, Dst = Self> {}

pub(crate) fn encode<T: Persist>(value: &T) -> Result<Vec<u8>> {
    Ok(wincode::config::serialize(value, CONFIG)?)
}

/// Measure a retained value without allocating or writing its serialized representation.
pub(crate) fn encoded_size<T: Persist>(value: &T) -> Result<u64> {
    Ok(wincode::config::serialized_size(value, CONFIG)?)
}

/// Decode exactly one owned value. Reject unconsumed bytes instead of silently accepting only
/// a prefix of the saved encoding.
pub(crate) fn decode<T: Persist>(bytes: &[u8]) -> Result<T> {
    Ok(wincode::config::deserialize_exact(bytes, CONFIG)?)
}

/// Read the leading field of a larger encoding. Record headers use this to recover output
/// versions without allocating the dependency list that follows them.
pub(crate) fn decode_prefix<T: Persist>(bytes: &[u8]) -> Result<T> {
    Ok(wincode::config::deserialize(bytes, CONFIG)?)
}

/// Send encoded bytes directly to a writer. Value storage uses this to fill blocks, and input
/// stamping uses it to feed a hash; neither needs a temporary Vec containing the whole encoding.
pub(crate) fn encode_into<T: Persist>(writer: impl wincode::io::Writer, value: &T) -> Result<()> {
    Ok(wincode::config::serialize_into(writer, value, CONFIG)?)
}
