//! Encoding policy for the bytes produced by a caller's serializer.
//!
//! Compression happens before the storage writer needs the value. Callers can therefore do this
//! CPU work concurrently, while one writer appends finished buffers sequentially. Each value is
//! independent: changing the policy affects future writes without invalidating old frames.

use std::io::{Read, Write};

use wincode::{SchemaRead, SchemaWrite};

use crate::{Error, Result};

pub(crate) const INLINE_BYTES: usize = 1024;

/// The space/CPU choice for new values. Readers use the codec saved with each value.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub enum Compression {
    /// Store the caller's bytes directly.
    None,
    /// LZ4 frames: prioritize inexpensive encoding and decoding.
    #[default]
    Lz4,
    /// Zstd frames at level three: spend more encoding work on smaller storage.
    Zstd,
}

impl Compression {
    /// Compress one already serialized value. Tiny values remain raw and inline in the index.
    /// Also keep the original if compression saves less than one eighth: a marginal reduction
    /// should not impose a decompressor on every future load.
    pub fn encode(self, bytes: Vec<u8>) -> Result<EncodedValue> {
        let raw_length = bytes.len() as u64;
        let mut compression = Self::None;
        let mut encoded = bytes;
        if encoded.len() > INLINE_BYTES && self != Self::None {
            let compressed = match self {
                Self::None => unreachable!(),
                Self::Lz4 => {
                    let mut writer = lz4_flex::frame::FrameEncoder::new(Vec::new());
                    writer.write_all(&encoded)?;
                    writer.finish()?
                }
                Self::Zstd => zstd::stream::encode_all(encoded.as_slice(), 3)?,
            };
            if compressed.len() < encoded.len() - encoded.len() / 8 {
                encoded = compressed;
                compression = self;
            }
        }
        Ok(EncodedValue {
            raw_length,
            compression,
            checksum: *blake3::hash(&encoded).as_bytes(),
            bytes: encoded,
        })
    }

    /// Bound decompression by the length published with the value, then require an exact result.
    /// The stored checksum is checked by the reader before it reaches this function.
    pub(crate) fn decode(self, bytes: Vec<u8>, raw_length: u64) -> Result<Vec<u8>> {
        if self == Self::None {
            if !(bytes.len() as u64 == raw_length) {
                return Err(Error::invalid_cache("invalid raw value length"));
            }
            return Ok(bytes);
        }
        let mut result = Vec::with_capacity(usize::try_from(raw_length)?);
        let limit = raw_length
            .checked_add(1)
            .ok_or_else(|| Error::invalid_cache("invalid value length"))?;
        match self {
            Self::None => unreachable!(),
            Self::Lz4 => {
                lz4_flex::frame::FrameDecoder::new(bytes.as_slice())
                    .take(limit)
                    .read_to_end(&mut result)
                    .map_err(Error::invalid_source)?;
            }
            Self::Zstd => {
                zstd::stream::Decoder::new(bytes.as_slice())
                    .map_err(Error::invalid_source)?
                    .take(limit)
                    .read_to_end(&mut result)
                    .map_err(Error::invalid_source)?;
            }
        }
        if !(result.len() as u64 == raw_length) {
            return Err(Error::invalid_cache("invalid decoded value length"));
        }
        Ok(result)
    }
}

/// An owned value ready to append. Its checksum covers the stored bytes, before decompression.
/// Keeping this separate from a transaction lets a caller prepare a bounded batch in parallel.
pub struct EncodedValue {
    pub(crate) raw_length: u64,
    pub(crate) compression: Compression,
    pub(crate) checksum: [u8; 32],
    pub(crate) bytes: Vec<u8>,
}

impl EncodedValue {
    pub fn raw_length(&self) -> u64 {
        self.raw_length
    }
}
