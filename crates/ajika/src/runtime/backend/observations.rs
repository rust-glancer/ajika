//! Share identical reads among the query records being written together. The temporary
//! dictionary goes away after this batch. Only its referenced entries survive on disk.

use std::{borrow::Cow, collections::HashMap};

use ajika_storage::{Read, Write};

use super::{Backend, MetadataWrite, Store};
use crate::{
    Error, Result, Statistics, codec,
    runtime::record::{Dependency, Record},
};

impl Backend {
    pub(crate) fn observations(&self) -> Observations<'_> {
        Observations {
            backend: self,
            snapshot: None,
        }
    }
}

/// Most reads are inline or already checked. Open a snapshot on the first ID that needs
/// resolving, then reuse it for the rest of the walk. Drop it before starting a checkpoint.
pub(crate) struct Observations<'db> {
    backend: &'db Backend,
    snapshot: Option<Read<'db>>,
}

impl Observations<'_> {
    pub(crate) fn resolve<'a>(
        &mut self,
        dependency: &'a Dependency,
    ) -> Result<Cow<'a, Dependency>> {
        let Dependency::Shared(id) = dependency else {
            return Ok(Cow::Borrowed(dependency));
        };
        let Store::Disk(database) = &self.backend.store else {
            unreachable!("only disk records reference shared observations");
        };
        self.backend.observe((|| {
            let read = match &self.snapshot {
                Some(read) => read,
                None => self.snapshot.insert(database.snapshot()?),
            };
            let observation = codec::decode(&read.shared_metadata(*id)?)?;
            if matches!(observation, Dependency::Shared(_)) {
                return Err(Error::invalid_cache(
                    "shared observation references another observation",
                ));
            }
            Ok(Cow::Owned(observation))
        })())
    }
}

impl Record {
    /// Replacing or evicting a record releases its share of each dictionary entry.
    pub(super) fn count_shared(&self, released: &mut HashMap<u32, u32>) {
        for dependency in self.dependencies.iter() {
            if let Dependency::Shared(id) = dependency {
                *released.entry(*id).or_default() += 1;
            }
        }
    }
}

impl MetadataWrite {
    pub(super) fn write_group(
        group: &[Self],
        write: &mut Write<'_>,
        stats: &mut Statistics,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        // Count exact observations, including the version. Two callers reading the same
        // slot at different versions must still validate independently after reopening.
        let mut counts = HashMap::<&Dependency, u32>::new();
        if group.len() > 1 {
            for row in group {
                check()?;
                for dep in row.record.dependencies.iter() {
                    *counts.entry(dep).or_default() += 1;
                }
            }
        }
        let mut shared = HashMap::new();
        let reference_size = codec::encoded_size(&Dependency::Shared(0))?;
        for (dep, count) in counts {
            if count < 2 {
                continue;
            }
            // A dictionary entry has a four-byte key, a four-byte count and some redb
            // page overhead. Leave room for that overhead before choosing to share.
            let size = codec::encoded_size(dep)?;
            let inline_bytes = u64::from(count) * size;
            let shared_bytes = size + 8 + u64::from(count) * reference_size;
            if inline_bytes > shared_bytes + 12 {
                check()?;
                let bytes = codec::encode(dep)?;
                if let Some(id) = write.share_metadata(&bytes, count)? {
                    shared.insert(dep, id);
                    stats.metadata_bytes_written += bytes.len() as u64 + 8;
                }
            }
        }

        for row in group {
            check()?;
            // Borrow the original record when every read stays inline. Otherwise replace
            // only this record's list, preserving first-read order and the retention footer.
            let mut packed;
            let record = if !shared.is_empty()
                && row
                    .record
                    .dependencies
                    .iter()
                    .any(|dep| shared.contains_key(dep))
            {
                packed = row.record.as_ref().clone();
                packed.dependencies = packed
                    .dependencies
                    .iter()
                    .map(|dep| {
                        shared
                            .get(dep)
                            .map_or_else(|| dep.clone(), |&id| Dependency::Shared(id))
                    })
                    .collect();
                &packed
            } else {
                row.record.as_ref()
            };
            let bytes = codec::encode(record)?;
            write.set_metadata(&row.id.0, &bytes)?;
            stats.metadata_writes += 1;
            stats.metadata_bytes_written += bytes.len() as u64;
        }
        Ok(())
    }
}
