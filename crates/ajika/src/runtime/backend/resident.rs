//! Two kinds of results can stay in RAM between runs. Think of an editor asking the
//! same query about a saved file and then an unsaved edit (a draft). The saved result
//! may be waiting for a disk write. The editor marks the unsaved text as a
//! [`Session`](crate::Lifetime::Session) input so its result can be reused on the next
//! keystroke. The backend keeps pending writes and session results in separate maps.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use super::Stored;
use crate::{
    Retention,
    runtime::{
        record::{QueryId, Record, RecordHeader, Slot},
        value::Value,
    },
};

struct Entry {
    stored: Stored,
    used: u64,
    verified: u64,
}

#[derive(Default)]
pub(super) struct ResidentStore {
    entries: HashMap<QueryId, Entry>,
    bytes: u64,
    last_collection: u64,
}

impl ResidentStore {
    /// A query may have been read again while waiting for a disk write. Save that last
    /// use so collection after a restart knows how recently the result was needed.
    pub(super) fn entries(&self) -> impl Iterator<Item = (QueryId, &Stored, u64)> {
        self.entries
            .iter()
            .map(|(id, entry)| (*id, &entry.stored, entry.used))
    }

    pub(super) fn contains(&self, id: &QueryId) -> bool {
        self.entries.contains_key(id)
    }

    pub(super) fn verified(&self, id: &QueryId) -> Option<u64> {
        self.entries.get(id).map(|entry| entry.verified)
    }

    pub(super) fn set_verified(&mut self, id: &QueryId, revision: u64) -> bool {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.verified = revision;
            return true;
        }
        false
    }

    pub(super) fn record(&self, id: &QueryId) -> Option<Arc<Record>> {
        self.entries
            .get(id)
            .map(|entry| entry.stored.record.clone())
    }

    pub(super) fn value(&self, id: &QueryId, header: &RecordHeader, slot: &Slot) -> Option<Value> {
        let stored = &self.entries.get(id)?.stored;
        if stored.record.header.values.get(slot)?.version != header.values.get(slot)?.version {
            return None;
        }
        stored.values.get(slot).cloned()
    }

    /// An operation can reuse a query without reading all of its outputs. When building
    /// its next record, carry over any missing values we already have for those versions.
    pub(super) fn fill_values(
        &self,
        id: &QueryId,
        header: &RecordHeader,
        values: &mut BTreeMap<Slot, Value>,
    ) {
        if let Some(entry) = self.entries.get(id) {
            for (slot, value) in &entry.stored.values {
                let previous = &entry.stored.record.header.values[slot];
                if header
                    .values
                    .get(slot)
                    .is_some_and(|current| current.version == previous.version)
                {
                    values.entry(slot.clone()).or_insert_with(|| value.clone());
                }
            }
        }
    }

    pub(super) fn insert(&mut self, id: QueryId, stored: Stored, now: u64) {
        self.remove(&id);
        self.bytes += stored.record.footer.bytes;
        self.entries.insert(
            id,
            Entry {
                stored,
                used: now,
                verified: 0,
            },
        );
    }

    pub(super) fn remove(&mut self, id: &QueryId) {
        if let Some(entry) = self.entries.remove(id) {
            self.bytes -= entry.stored.record.footer.bytes;
        }
    }

    pub(super) fn touch(&mut self, id: &QueryId, now: u64) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.used = now;
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
        // Sort IDs and timestamps so choosing what to evict doesn't copy the query results.
        let mut ordered = self
            .entries
            .iter()
            .map(|(id, entry)| (*id, entry.used))
            .collect::<Vec<_>>();
        ordered.sort_by_key(|(id, used)| (*used, *id));
        let mut removed = 0;
        for (id, used) in ordered {
            if self.entries.len() > retention.max_entries
                || self.bytes > retention.max_bytes
                || now.saturating_sub(used) > max_idle
            {
                self.remove(&id);
                removed += 1;
            }
        }
        if self.entries.len() < self.entries.capacity() / 4 {
            self.entries.shrink_to_fit();
        }
        removed
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}
