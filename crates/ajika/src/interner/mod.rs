//! Interning immutable objects as compact IDs, independent of query validity.
//!
//! A file path keeps its identity when its contents change, when a query is collected, or when
//! a different worker discovers it first. The host can share this interner with its captured
//! inputs. New assignments stay in memory until a durable query checkpoint saves them alongside
//! the values that may refer to them. They are never rolled back or reused within a live database.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use ajika_storage::Write;

use crate::{Error, Persist, Result, UnavailableData, codec, error::ensure};

/// A database's shared interner for immutable objects.
///
/// Use an [`interned`](crate::interned) declaration to name an entity by its immutable fields.
/// Each declaration has one numbering space per database. Include context in the declaration's
/// fields when it is part of the entity's identity. Cloned handles share all assignments.
/// Unlike query results, an assignment has no dependencies and never changes after publication.
///
/// The handle returned by [`Database::interner`](crate::Database::interner) shares that database's
/// assignments. An ID is only meaningful with its interner: another database can assign the same
/// number to different fields.
///
/// New assignments are saved alongside query data at a durable checkpoint; allocating an ID
/// alone doesn't save it to disk. Reopening a compatible cache restores the saved assignments.
/// IDs are never recycled within a live interner. Query collection and
/// [`Database::invalidate`](crate::Database::invalidate) preserve them, but a full cache reset,
/// such as an epoch change, can discard assignments together with the query data referring to them.
#[derive(Clone, Default)]
pub struct Interner {
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for Interner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock().expect("interner state");
        f.debug_struct("Interner")
            .field("namespaces", &state.namespaces.len())
            .field("key_bytes", &state.bytes)
            .finish()
    }
}

impl Interner {
    /// Return the same ID for equal keys in this namespace. Concurrent discoveries share one
    /// assignment. Allocation does not open a transaction or mark any query as changed.
    fn intern(&self, namespace: String, key: &[u8]) -> Result<u32> {
        let mut state = self.state.lock().expect("interner state");
        let entries = state.namespaces.entry(namespace).or_default();
        if let Some(id) = entries.ids.get(key) {
            return Ok(*id);
        }
        let id = u32::try_from(entries.keys.len())?;
        let key: Arc<[u8]> = Arc::from(key);
        entries.keys.push(key.clone());
        let bytes = key.len();
        entries.ids.insert(key, id);
        state.bytes += bytes;
        Ok(id)
    }

    /// Recover an immutable key without consulting or validating a query result.
    fn key(&self, namespace: &str, id: u32) -> Option<Arc<[u8]>> {
        self.state
            .lock()
            .expect("interner state")
            .namespaces
            .get(namespace)?
            .keys
            .get(id as usize)
            .cloned()
    }

    /// Approximate owned memory, including lookup tables but excluding cloned handles.
    pub fn memory_bytes(&self) -> usize {
        let state = self.state.lock().expect("interner state");
        state.bytes
            + state
                .namespaces
                .iter()
                .map(|(name, entries)| {
                    name.capacity()
                        + entries.keys.capacity() * std::mem::size_of::<Arc<[u8]>>()
                        + entries.ids.capacity() * (std::mem::size_of::<(Arc<[u8]>, u32)>() + 1)
                        + entries.keys.len() * 2 * std::mem::size_of::<usize>()
                })
                .sum::<usize>()
    }

    /// Refresh key allocations after releasing query working values. Existing handles stay valid.
    pub(crate) fn reallocate(&self) {
        let started = Instant::now();
        let mut state = self.state.lock().expect("interner state");
        // Moving long-lived keys can free allocator pages that also held temporary discovery data.
        for entries in state.namespaces.values_mut() {
            // Both indexes own the old keys: replacing just one would leave them pinned.
            // Preserve vector order (the IDs) and the saved prefix used by checkpointing.
            entries.keys = entries
                .keys
                .iter()
                .map(|key| Arc::<[u8]>::from(key.as_ref()))
                .collect();
            entries.ids = entries
                .keys
                .iter()
                .enumerate()
                .map(|(id, key)| (key.clone(), id as u32))
                .collect();
        }
        tracing::debug!(
            elapsed_us = started.elapsed().as_micros(),
            key_bytes = state.bytes,
            "relocated identity keys"
        );
    }

    pub(crate) fn load(rows: Vec<(String, u32, Vec<u8>)>) -> Result<Self> {
        let interner = Self::default();
        for (namespace, expected, key) in rows {
            ensure!(
                interner.intern(namespace, &key)? == expected,
                Error::invalid_cache("invalid identity sequence"),
            );
        }
        for entries in interner
            .state
            .lock()
            .expect("interner state")
            .namespaces
            .values_mut()
        {
            entries.saved = entries.keys.len();
        }
        Ok(interner)
    }

    /// Write pending assignments while the operation is drained. A failed commit leaves saved
    /// lengths untouched; the next successful checkpoint can write the same assignments again.
    pub(crate) fn write_pending(&self, write: &Write<'_>) -> Result<Vec<(String, usize)>> {
        let state = self.state.lock().expect("interner state");
        let mut saved = Vec::new();
        for (name, entries) in &state.namespaces {
            for (id, key) in entries.keys.iter().enumerate().skip(entries.saved) {
                write.set_identity(name, id as u32, key)?;
            }
            if entries.saved != entries.keys.len() {
                saved.push((name.clone(), entries.keys.len()));
            }
        }
        Ok(saved)
    }

    pub(crate) fn checkpointed(&self, saved: Vec<(String, usize)>) {
        let mut state = self.state.lock().expect("interner state");
        for (name, count) in saved {
            state
                .namespaces
                .get_mut(&name)
                .expect("checkpointed namespace")
                .saved = count;
        }
    }
}

/// Connect an interned declaration's generated ID and fields to the shared interner.
///
/// This is macro plumbing, re-exported through `__private`. Encoding happens before acquiring
/// the state lock and decoding after releasing it. Reading fields creates owned values;
/// it neither retains a decoded copy nor records a query dependency.
pub trait Interned: Copy {
    type Fields: Persist;
    const NAMESPACE: &'static str;

    fn from_raw(raw: u32) -> Self;

    fn as_raw(self) -> u32;

    fn intern(interner: &Interner, fields: Self::Fields) -> Result<Self> {
        let key = codec::encode(&fields)?;
        interner
            .intern(Self::NAMESPACE.to_owned(), &key)
            .map(Self::from_raw)
    }

    fn fields(self, interner: &Interner) -> Result<Self::Fields> {
        let namespace = Self::NAMESPACE;
        let id = self.as_raw();
        let key = interner
            .key(namespace, id)
            .ok_or(UnavailableData::Identity { namespace, id })?;
        codec::decode(&key)
    }
}

#[derive(Default)]
struct State {
    namespaces: HashMap<String, Namespace>,
    bytes: usize,
}

#[derive(Default)]
struct Namespace {
    // Both indexes share the key allocation. The vector supplies insertion order for checkpointing.
    keys: Vec<Arc<[u8]>>,
    ids: HashMap<Arc<[u8]>, u32>,
    saved: usize,
}
