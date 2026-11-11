//! Completed editor work retained over the durable cache.
//!
//! A session record replaces the producer's complete output metadata, including removals.
//! Only payloads whose versions differ from durable storage need resident values. Equal slots
//! keep their disk lengths and can be loaded later without retaining their decoded objects.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use super::{Stored, Usage};
use crate::{
    Result, Retention, codec,
    runtime::{
        record::{QueryId, Record, RecordHeader, Slot},
        value::Value,
    },
};

#[derive(Default)]
pub(super) struct SessionStore {
    entries: HashMap<QueryId, Stored>,
    usage: HashMap<QueryId, Usage>,
    bytes: u64,
    last_collection: u64,
}

impl SessionStore {
    pub(super) fn verified(&self, id: &QueryId) -> Option<u64> {
        self.usage.get(id).map(|entry| entry.verified)
    }

    pub(super) fn set_verified(&mut self, id: &QueryId, revision: u64) {
        if let Some(entry) = self.usage.get_mut(id) {
            entry.verified = revision;
        }
    }

    pub(super) fn record(&self, id: &QueryId) -> Option<Arc<Record>> {
        self.entries.get(id).map(|entry| entry.record.clone())
    }

    pub(super) fn value(&self, id: &QueryId, header: &RecordHeader, slot: &Slot) -> Option<Value> {
        let entry = self.entries.get(id)?;
        if entry.record.header.values.get(slot)?.version != header.values.get(slot)?.version {
            return None;
        }
        entry.values.get(slot).cloned()
    }

    pub(super) fn values(&self, id: &QueryId, header: &RecordHeader) -> BTreeMap<Slot, Value> {
        header
            .values
            .keys()
            .filter_map(|slot| {
                self.value(id, header, slot)
                    .map(|value| (slot.clone(), value))
            })
            .collect()
    }

    /// Prepare an override without publishing it. Measuring sizes invokes the value's schema,
    /// so the caller runs this outside storage locks and installs it only after checkpoint success.
    pub(super) fn prepare(
        mut record: Record,
        mut values: BTreeMap<Slot, Value>,
        durable: Option<&Record>,
        disk: bool,
    ) -> Result<Stored> {
        let mut bytes = 0;
        for (slot, output) in &mut Arc::make_mut(&mut record.header).values {
            let backing = durable
                .and_then(|old| old.header.values.get(slot))
                .filter(|old| old.version == output.version);
            if let Some(backing) = backing
                && (!disk || backing.length.is_some())
            {
                output.length = backing.length;
                values.remove(slot);
            } else {
                output.length = None;
                if let Some(value) = values.get(slot)
                    && let Some(codec) = value.codec
                {
                    bytes += (codec.size)(&value.data)?;
                } else {
                    values.remove(slot);
                }
            }
        }
        values.retain(|slot, _| record.header.values.contains_key(slot));
        record.bytes = bytes + codec::encoded_size(&record)?;
        Ok(Stored {
            record: Arc::new(record),
            values,
        })
    }

    pub(super) fn insert(&mut self, id: QueryId, stored: Stored, now: u64) {
        self.remove(&id);
        self.bytes += stored.record.bytes;
        self.usage.insert(
            id,
            Usage {
                used: now,
                bytes: stored.record.bytes,
                verified: 0,
            },
        );
        self.entries.insert(id, stored);
    }

    pub(super) fn remove(&mut self, id: &QueryId) {
        self.entries.remove(id);
        if let Some(usage) = self.usage.remove(id) {
            self.bytes -= usage.bytes;
        }
    }

    pub(super) fn touch(&mut self, id: &QueryId, now: u64) {
        if let Some(usage) = self.usage.get_mut(id) {
            usage.used = now;
        }
    }

    pub(super) fn usage(&self) -> (u64, u64) {
        (self.entries.len() as u64, self.bytes)
    }

    pub(super) fn collect(&mut self, retention: Retention, now: u64) -> usize {
        let max_idle = retention.max_idle.as_millis().min(u64::MAX as u128) as u64;
        if self.entries.len() <= retention.max_entries
            && self.bytes <= retention.max_bytes
            && now.saturating_sub(self.last_collection) < max_idle.min(1000)
        {
            return 0;
        }
        self.last_collection = now;
        let mut ordered = self
            .usage
            .iter()
            .map(|(id, usage)| (*id, *usage))
            .collect::<Vec<_>>();
        ordered.sort_by_key(|(id, usage)| (usage.used, *id));
        let mut removed = 0;
        for (id, usage) in ordered {
            if self.entries.len() > retention.max_entries
                || self.bytes > retention.max_bytes
                || now.saturating_sub(usage.used) > max_idle
            {
                self.remove(&id);
                removed += 1;
            }
        }
        if self.entries.len() < self.entries.capacity() / 4 {
            self.entries.shrink_to_fit();
            self.usage.shrink_to_fit();
        }
        removed
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}
