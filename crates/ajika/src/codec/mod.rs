//! The serialization contract shared by query declarations and persistent storage.
//!
//! [`Persist`] describes values Ajika can encode and reconstruct as owned Rust objects. The helpers
//! use the same wincode configuration for saved keys, dependency records, and result payloads.
//! Storage decides how those bytes are buffered or split into blocks; this module keeps their
//! encoding consistent and independent of the particular storage operation.

use wincode::{SchemaReadOwned, SchemaWrite};

use crate::{Error, Result};

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
    wincode::config::serialize(value, CONFIG).map_err(Error::value_encode)
}

/// Measure a retained value without allocating or writing its serialized representation.
pub(crate) fn encoded_size<T: Persist>(value: &T) -> Result<u64> {
    wincode::config::serialized_size(value, CONFIG).map_err(Error::value_encode)
}

/// Decode exactly one owned value. Reject unconsumed bytes instead of silently accepting only
/// a prefix of the saved encoding.
pub(crate) fn decode<T: Persist>(bytes: &[u8]) -> Result<T> {
    // These bytes belong to Ajika's representation. A provider decoding its own input instead
    // uses Error::application; the codec dependency alone cannot determine recovery policy.
    wincode::config::deserialize_exact(bytes, CONFIG).map_err(Error::cache_decode)
}

/// Read the leading field of a larger encoding. Record headers use this to recover output
/// versions without allocating the dependency list that follows them.
pub(crate) fn decode_prefix<T: Persist>(bytes: &[u8]) -> Result<T> {
    wincode::config::deserialize(bytes, CONFIG).map_err(Error::cache_decode)
}

/// Send encoded bytes directly to a writer. Value storage uses this to fill blocks, and input
/// stamping uses it to feed a hash; neither needs a temporary Vec containing the whole encoding.
pub(crate) fn encode_into<T: Persist>(writer: impl wincode::io::Writer, value: &T) -> Result<()> {
    wincode::config::serialize_into(writer, value, CONFIG).map_err(Error::value_encode)
}

#[cfg(test)]
mod tests {
    use crate::{ErrorKind, PersistenceErrorKind};

    #[test]
    fn internal_decoding_retains_the_cache_category_and_original_cause() {
        for result in [super::decode::<u64>(&[]), super::decode_prefix::<u64>(&[])] {
            let error = result.expect_err("an integer requires encoded bytes");
            assert!(
                matches!(error.kind(), ErrorKind::Persistence(error) if error.kind() == PersistenceErrorKind::InvalidCache)
            );
            assert!(
                std::error::Error::source(&error)
                    .expect("original codec error")
                    .is::<wincode::ReadError>()
            );
        }
    }
}
