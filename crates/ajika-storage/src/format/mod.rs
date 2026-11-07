//! Shared index and pack descriptions. Physical locations stay below the caller's record format.
//!
//! A logical value ID survives replacement. A pack ID never gets reused, and a location names
//! one immutable frame. This distinction lets later compaction move bytes without changing
//! anything in the caller's dependency records.

use redb::TableDefinition;
use wincode::{SchemaRead, SchemaWrite};

use crate::{Compression, Id};

pub(crate) const FORMAT: &str = "ajika-packs-2";
pub(crate) const HEADER: TableDefinition<&str, &[u8]> = TableDefinition::new("header");
pub(crate) const RECORDS: TableDefinition<&Id, &[u8]> = TableDefinition::new("records");
pub(crate) const IDENTITIES: TableDefinition<(&str, u32), &[u8]> =
    TableDefinition::new("identities");
pub(crate) const VALUES: TableDefinition<&Id, &[u8]> = TableDefinition::new("values");
pub(crate) const PACKS: TableDefinition<&PackId, &[u8]> = TableDefinition::new("packs");

pub(crate) type PackId = [u8; 16];

#[derive(SchemaRead, SchemaWrite)]
pub(crate) enum Location {
    Inline(Vec<u8>),
    Pack {
        pack: PackId,
        offset: u64,
        header: FrameHeader,
    },
}

/// A sealed file's inventory. Live counts change transactionally as locations are replaced.
/// The file length does not change; its dead bytes remain until the pack can be reclaimed.
#[derive(Debug, Clone, Copy, SchemaRead, SchemaWrite)]
pub(crate) struct PackInfo {
    pub(crate) length: u64,
    pub(crate) live_bytes: u64,
    pub(crate) live_values: u64,
    /// All frames before this offset have been evacuated or were already dead. Persisting the
    /// cursor with relocated values lets a later pass resume without rescanning the prefix.
    pub(crate) scan_offset: u64,
}

impl PackInfo {
    pub(crate) fn dead_bytes(&self) -> u64 {
        self.length - PackHeader::LENGTH - self.live_bytes
    }
}

#[derive(Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub(crate) struct PackHeader {
    pub(crate) magic: [u8; 8],
    pub(crate) instance: PackId,
    pub(crate) pack: PackId,
}

impl PackHeader {
    pub(crate) const MAGIC: [u8; 8] = *b"AJKPACK1";
    pub(crate) const LENGTH: u64 = 40;
}

/// Framing is also enough to walk a pack during future compaction. Checking its ID/offset in
/// VALUES tells whether a frame is live; no second resident per-pack lookup table is required.
#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub(crate) struct FrameHeader {
    pub(crate) id: Id,
    pub(crate) raw_length: u64,
    pub(crate) stored_length: u64,
    pub(crate) compression: Compression,
    pub(crate) checksum: [u8; 32],
}

impl FrameHeader {
    // wincode stores the enum discriminant as a u32. The remaining fields have fixed widths.
    pub(crate) const LENGTH: u64 = 32 + 8 + 8 + 4 + 32;

    pub(crate) fn frame_length(&self) -> u64 {
        Self::LENGTH + self.stored_length
    }
}

pub(crate) fn new_id() -> crate::Result<PackId> {
    let mut id = [0; 16];
    getrandom::fill(&mut id)?;
    Ok(id)
}
