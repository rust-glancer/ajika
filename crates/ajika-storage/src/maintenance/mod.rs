//! Reclaim obsolete bytes without loading query results.
//!
//! Replacing a location leaves its old frame in an immutable file. A maintenance pass walks
//! selected files and copies only frames still named by the index. The new locations and the
//! scan position commit together. Every committed prefix is therefore safe to skip after a
//! restart, and a file disappears only when its last live location has moved or been removed.

use std::{
    cmp::Reverse,
    collections::BTreeMap,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    sync::atomic::Ordering,
    time::Instant,
};

use redb::ReadableTable;

use crate::{
    Database, Error, Result, Write,
    error::ensure,
    format::{self, FrameHeader, Location, PACKS, PackHeader, PackInfo, VALUES},
    pack::PackWriter,
};

pub(crate) const PASS_BYTES: u64 = 16 * 1024 * 1024;
const PASS_FRAMES: u64 = 4096;
const SMALL_PACK_BYTES: u64 = PASS_BYTES / 4;
const SMALL_PACK_COUNT: usize = 8;

impl Database {
    /// Run one small reclamation pass between operations if needed. A single large value can
    /// make the pass exceed its usual work budget.
    pub fn maintain(&self) -> Result<()> {
        if self.maintenance_due.swap(false, Ordering::Relaxed) {
            self.compact_step(false)?;
        }
        Ok(())
    }

    pub(crate) fn compact_step(&self, force: bool) -> Result<bool> {
        let started = Instant::now();
        let mut write = Write::maintenance(self)?;
        let inventory = write
            .transaction
            .open_table(PACKS)?
            .iter()?
            .map(|row| {
                let (id, bytes) = row?;
                Ok((*id.value(), format::decode::<PackInfo>(bytes.value())?))
            })
            .collect::<Result<Vec<_>>>()?;

        // Finish interrupted evacuations first. Otherwise require worthwhile dead space,
        // except under capacity pressure or during an explicit full compaction.
        let pressure = self.write_budget() == 0;
        let mut candidates = inventory
            .iter()
            .copied()
            .filter(|(_, info)| {
                info.scan_offset > PackHeader::LENGTH
                    || (info.dead_bytes() > 0
                        && (force
                            || pressure
                            || (info.dead_bytes() >= 1024 * 1024
                                && info.dead_bytes() >= info.length / 4)))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(id, info)| {
            (
                info.scan_offset == PackHeader::LENGTH,
                Reverse(info.dead_bytes()),
                *id,
            )
        });

        // A new body per request can leave many tiny, entirely live packs. Merge a group of
        // those too. Both byte and frame counts must be well below a pass's limits; otherwise
        // a dense output pack could consume the next pass alone and be copied indefinitely.
        if candidates.is_empty() {
            let small = inventory
                .into_iter()
                .filter(|(_, info)| {
                    info.length < SMALL_PACK_BYTES && info.live_values < PASS_FRAMES / 4
                })
                .collect::<Vec<_>>();
            if force && small.len() >= 2 {
                candidates = small;
            } else if !force {
                // Merge similarly sized files. Otherwise every few tiny saves would copy the
                // same growing pack again. Fourfold size bands keep that write amplification
                // small without introducing persistent levels or another index.
                let mut bands = BTreeMap::<_, Vec<_>>::new();
                for (id, info) in small {
                    bands
                        .entry(info.length.ilog2() / 2)
                        .or_default()
                        .push((id, info));
                }
                candidates = bands
                    .into_values()
                    .find(|band| band.len() >= SMALL_PACK_COUNT)
                    .unwrap_or_default();
            }
        }
        if candidates.is_empty() {
            return Ok(false);
        }

        let mut scanned = 0;
        let mut frames = 0;
        let mut copied = 0;
        let mut moved = 0;
        let mut retired = 0;
        for (id, info) in candidates {
            let mut source = BufReader::new(File::open(PackWriter::path(&self.packs, &id))?);
            let mut offset = info.scan_offset;
            source.seek(SeekFrom::Start(offset))?;
            while offset < info.length && scanned < PASS_BYTES && frames < PASS_FRAMES {
                let mut bytes = [0; FrameHeader::LENGTH as usize];
                source.read_exact(&mut bytes)?;
                let header: FrameHeader = format::decode(&bytes)?;
                let end = offset
                    .checked_add(FrameHeader::LENGTH)
                    .and_then(|start| start.checked_add(header.stored_length))
                    .ok_or_else(|| Error::invalid_cache("invalid pack frame range"))?;
                ensure!(
                    end <= info.length,
                    Error::invalid_cache("invalid pack frame range"),
                );
                let live = {
                    let table = write.transaction.open_table(VALUES)?;
                    let location = table
                        .get(&header.id)?
                        .map(|bytes| format::decode::<Location>(bytes.value()))
                        .transpose()?;
                    match location {
                        Some(Location::Pack {
                            pack,
                            offset: at,
                            header: expected,
                        }) if pack == id && at == offset => {
                            ensure!(
                                header == expected,
                                Error::invalid_cache("invalid stored frame header"),
                            );
                            true
                        }
                        _ => false,
                    }
                };
                if live {
                    let value_id = header.id;
                    copied += end - offset;
                    let location = write.packs.copy_frame(header, &mut source)?;
                    write.replace_location(&value_id, location)?;
                    moved += 1;
                } else {
                    source.seek(SeekFrom::Start(end))?;
                }
                scanned += end - offset;
                frames += 1;
                offset = end;
            }

            let remaining = write.changed_packs.entry(id).or_insert(info);
            remaining.scan_offset = offset;
            // A damaged dead-frame length could skip a live frame. Never retire such a file:
            // its inventory must agree that all live locations have actually moved.
            ensure!(
                offset < info.length || remaining.live_values == 0,
                Error::invalid_cache("pack inventory contains unreachable frames"),
            );
            if remaining.live_values == 0 {
                retired += 1;
            }
            if scanned >= PASS_BYTES || frames >= PASS_FRAMES {
                break;
            }
        }
        write.commit()?;
        tracing::debug!(scanned, copied, moved, retired, elapsed = ?started.elapsed(),
            "packed storage maintenance finished");
        Ok(true)
    }
}
