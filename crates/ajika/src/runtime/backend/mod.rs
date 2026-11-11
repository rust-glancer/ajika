//! Keeping query work for reuse after the operation that computed it has finished.
//!
//! The evaluator needs to retrieve old records and values, save successful work, and discard
//! old results to respect retention limits. [`Backend`] provides those operations for disk and
//! typed memory storage. It understands queries and their output slots, while the lower storage
//! adapter handles opaque metadata and payload bytes. Deciding whether an old answer is valid
//! remains the evaluator's job.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    error::Error as _,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ajika_storage::{Database as StorageDatabase, Options};

use super::{
    cache::Memo,
    record::{Address, QueryId, Record, RecordHeader, Slot},
    value::{Codec, Value},
};
use crate::{Error, Lifetime, Result, Retention, Statistics, codec};

mod encoding;
mod session;
use self::{encoding::PendingValue, session::SessionStore};

/// A saved query in the memory backend. The backend owns these references after operation
/// cleanup clears the memo's working values, allowing persistent-policy answers to be reused.
struct Stored {
    record: Arc<Record>,
    values: BTreeMap<Slot, Value>,
}

/// Small process-local facts about a stored record. Retention needs its size and last use;
/// reuse needs the input revision in which it was checked. Keeping them together means a
/// collected or replaced record cannot leave an unrelated validity proof behind.
#[derive(Clone, Copy)]
struct Usage {
    used: u64,
    bytes: u64,
    // This proof belongs to the stored record, not a decoded working copy. Reopening starts
    // at zero; only a successful operation can establish validity for this database's inputs.
    verified: u64,
}

enum Store {
    Memory(Mutex<HashMap<QueryId, Stored>>),
    Disk(StorageDatabase),
}

/// The store of query results retained from completed operations.
///
/// Memos hold work being used or computed now. This store supplies the previous saved work
/// when those memos need it, and receives their changes only when an operation checkpoints.
/// Keeping those roles separate lets failed operations discard new work and lets successful
/// disk-backed operations release decoded values without losing their reusable results.
///
/// Both backends retain dependency records and eligible output payloads. Disk uses encoded
/// records and packed values; memory keeps typed values without serializing them. Retention bookkeeping
/// belongs here because collection must remove a query's record and all its outputs together.
pub(crate) struct Backend {
    pub(crate) interner: crate::Interner,
    session: Mutex<SessionStore>,
    store: Store,
    // Small retention inventory. Loading this does not populate the runtime's query memos.
    usage: Mutex<HashMap<QueryId, Usage>>,
    // Keep the first fault that needs repair until the next run has exclusive access.
    recovery: OnceLock<Error>,
    bytes: AtomicU64,
}

impl Backend {
    pub(crate) fn memory() -> Self {
        Self {
            interner: crate::Interner::default(),
            store: Store::Memory(Mutex::default()),
            session: Mutex::default(),
            usage: Mutex::default(),
            recovery: OnceLock::new(),
            bytes: AtomicU64::new(0),
        }
    }

    /// Open saved work and rebuild its retention inventory. This reads record metadata, not
    /// query payloads; memos and validity proofs are established later as queries are requested.
    pub(crate) fn open(path: &Path, epoch: &str) -> Result<Self> {
        // The runtime's record schema has its own version, separate from the storage
        // adapter and the application's persisted query types.
        let mut database = StorageDatabase::open(
            path,
            &format!("query-runtime-3:{epoch}"),
            Options::default(),
        )?;
        let (interner, usage) = match Self::load_interner(&mut database) {
            Ok(interner) => {
                let loaded = Self::load_usage(&database);
                let usage = Self::reuse_or_rebuild_usage(&mut database, &interner, loaded)?;
                (interner, usage)
            }
            Err(error) if error.is_invalid_cache() => {
                let usage = Self::rebuild_corrupt_cache(&mut database, None, &error)?;
                (crate::Interner::default(), usage)
            }
            Err(error) => return Err(error),
        };
        let bytes = usage.values().map(|item| item.bytes).sum();
        Ok(Self {
            interner,
            store: Store::Disk(database),
            session: Mutex::default(),
            usage: Mutex::new(usage),
            recovery: OnceLock::new(),
            bytes: AtomicU64::new(bytes),
        })
    }

    fn load_interner(database: &mut StorageDatabase) -> Result<crate::Interner> {
        let assignments = database.snapshot()?.identities()?;
        // At startup we can bound historical keys before callers receive their IDs. Reset
        // the saved queries with the interner because queries can refer to those IDs.
        // Live recovery keeps the existing assignments because callers already hold them.
        const MAX_INTERNER_BYTES: usize = 32 * 1024 * 1024;
        if assignments
            .iter()
            .map(|(namespace, _, key)| namespace.len() + key.len())
            .sum::<usize>()
            > MAX_INTERNER_BYTES
        {
            database.rebuild(&[])?;
            return Ok(crate::Interner::default());
        }
        crate::Interner::load(assignments)
    }

    fn reuse_or_rebuild_usage(
        database: &mut StorageDatabase,
        interner: &crate::Interner,
        loaded: Result<HashMap<QueryId, Usage>>,
    ) -> Result<HashMap<QueryId, Usage>> {
        match loaded {
            Err(error) if error.is_invalid_cache() => {
                Self::rebuild_corrupt_cache(database, Some(interner), &error)
            }
            result => result,
        }
    }

    fn rebuild_corrupt_cache(
        database: &mut StorageDatabase,
        interner: Option<&crate::Interner>,
        error: &Error,
    ) -> Result<HashMap<QueryId, Usage>> {
        match interner {
            Some(interner) => interner.rebuild(database)?,
            None => database.rebuild(&[])?,
        }
        tracing::warn!(
            %error,
            cause = error.source().map(tracing::field::display),
            path = %database.path().display(),
            identities_discarded = interner.is_none(),
            "discarded corrupt cache contents"
        );
        Ok(HashMap::new())
    }

    fn load_usage(database: &StorageDatabase) -> Result<HashMap<QueryId, Usage>> {
        let mut usage = HashMap::new();
        // Restart needs sizes and last-use times to enforce retention. Decode each record
        // only long enough to extract those fields; dependency lists are not retained here,
        // and query payloads are not loaded at all.
        for (id, bytes) in database.snapshot()?.records()? {
            let record: Record = codec::decode(&bytes)?;
            usage.insert(
                QueryId(id),
                Usage {
                    used: record.last_used,
                    bytes: record.bytes,
                    verified: 0,
                },
            );
        }
        Ok(usage)
    }

    pub(crate) fn recovery_pending(&self) -> bool {
        self.recovery.get().is_some()
    }

    pub(crate) fn recover(&mut self) -> Result<()> {
        let Some(reason) = self.recovery.get().cloned() else {
            return Ok(());
        };
        let Store::Disk(database) = &mut self.store else {
            unreachable!("recovery belongs to disk storage")
        };
        // Reopen after an uncertain commit to find the published checkpoint. Payload
        // checksums are verified on read, so known corruption goes straight to a rebuild.
        let interner = &self.interner;
        let usage = if reason.storage_needs_reopen() {
            let reopened = database.reopen().map_err(Error::from).and_then(|()| {
                interner.reconcile(&database.snapshot()?.identities()?)?;
                Self::load_usage(database)
            });
            Self::reuse_or_rebuild_usage(database, interner, reopened)?
        } else {
            Self::rebuild_corrupt_cache(database, Some(interner), &reason)?
        };
        // Leave repair pending until success so the next run can retry a temporary failure.
        self.replace_usage(usage);
        self.recovery.take();
        Ok(())
    }

    fn replace_usage(&mut self, usage: HashMap<QueryId, Usage>) {
        // A session record can point to a payload stored in a durable pack. Recovery may
        // restore an older checkpoint with different packs and records, so clear the
        // session cache. Saved queries will be validated against the installed inputs.
        self.session.get_mut().expect("session storage").clear();
        self.bytes.store(
            usage.values().map(|item| item.bytes).sum(),
            Ordering::Relaxed,
        );
        *self.usage.get_mut().expect("storage usage") = usage;
    }

    /// A last-use time that can be saved and compared after restarting. Process-local
    /// [`Instant`](std::time::Instant) values are suitable for scheduling maintenance, but not for
    /// the age of persisted query records.
    pub(crate) fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis()
            .min(u64::MAX as u128) as u64
    }

    pub(crate) fn set_compression(&mut self, compression: crate::Compression) {
        if let Store::Disk(database) = &mut self.store {
            database.set_compression(compression);
        }
    }

    pub(crate) fn storage_statistics(&self) -> Result<Option<crate::StorageStatistics>> {
        self.check()?;
        match &self.store {
            Store::Memory(_) => Ok(None),
            Store::Disk(database) => {
                self.observe(database.statistics().map(Some).map_err(Error::from))
            }
        }
    }

    pub(crate) fn check(&self) -> Result<()> {
        self.recovery
            .get()
            .map_or(Ok(()), |error| Err(error.clone()))
    }

    /// Record failures at the storage boundary so a callback returning its own error cannot
    /// hide a repair need. Ordinary failures leave the last committed checkpoint usable.
    fn observe<T>(&self, result: Result<T>) -> Result<T> {
        result.inspect_err(|error| {
            if self.is_disk() && (error.is_invalid_cache() || error.storage_needs_reopen()) {
                let _ = self.recovery.set(error.clone());
            }
        })
    }

    /// Load only the output metadata into a known-valid call's working memo. Disk storage
    /// still reads the complete metadata row, but decoding stops before dependency addresses.
    /// This keeps one stored record without allocating a dependency graph just to load a value.
    pub(crate) fn header(&self, id: &QueryId) -> Result<(Option<Arc<RecordHeader>>, bool)> {
        self.check()?;
        if let Some(record) = self.session.lock().expect("session storage").record(id) {
            return Ok((Some(record.header.clone()), false));
        }
        match &self.store {
            Store::Memory(store) => Ok((
                store
                    .lock()
                    .expect("memory storage")
                    .get(id)
                    .map(|stored| stored.record.header.clone()),
                false,
            )),
            Store::Disk(database) => self.observe((|| {
                Ok((
                    database
                        .snapshot()?
                        .metadata(&id.0)?
                        .map(|bytes| codec::decode_prefix(&bytes))
                        .transpose()?,
                    true,
                ))
            })()),
        }
    }

    /// Retrieve the last saved dependencies and output descriptions for a query call.
    /// No payload is loaded and no validity is assumed. The evaluator uses this record to
    /// check reuse or compare a new execution with the old one. A new or collected key is absent.
    /// The flag reports a disk read, including a miss, for the operation's I/O counters.
    pub(crate) fn record(&self, id: &QueryId) -> Result<(Option<Arc<Record>>, bool)> {
        self.check()?;
        if let Some(record) = self.session.lock().expect("session storage").record(id) {
            return Ok((Some(record), false));
        }
        Ok((self.durable_record(id)?, self.is_disk()))
    }

    fn durable_record(&self, id: &QueryId) -> Result<Option<Arc<Record>>> {
        match &self.store {
            Store::Memory(store) => Ok(store
                .lock()
                .expect("memory storage")
                .get(id)
                .map(|stored| stored.record.clone())),
            Store::Disk(database) => self.observe((|| {
                database
                    .snapshot()?
                    .metadata(&id.0)?
                    .map(|bytes| codec::decode(&bytes).map(Arc::new))
                    .transpose()
            })()),
        }
    }

    /// Retrieve one saved answer, either by sharing a typed memory value or decoding disk bytes.
    /// The supplied header describes the version being loaded; the evaluator decides whether
    /// it is a current answer or an old value needed for comparison. Absence is normal when the
    /// record has no retained payload. This never executes the query to fill such a gap.
    /// The flag distinguishes decoding from sharing a resident value, so session hits do not
    /// appear as disk reads in profiling.
    pub(crate) fn value(
        &self,
        address: &Address,
        header: &RecordHeader,
        slot: &Slot,
        codec: Codec,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<Option<(Value, bool)>> {
        self.check()?;
        if header.lifetime == Lifetime::Operation {
            return Ok(None);
        }
        if let Some(value) =
            self.session
                .lock()
                .expect("session storage")
                .value(&address.id(), header, slot)
        {
            return Ok(Some((value, false)));
        }
        match &self.store {
            Store::Memory(store) => Ok(store
                .lock()
                .expect("memory storage")
                .get(&address.id())
                .filter(|stored| {
                    stored.record.header.values.get(slot).map(|v| v.version)
                        == header.values.get(slot).map(|v| v.version)
                })
                .and_then(|stored| stored.values.get(slot))
                .cloned()
                .map(|value| (value, false))),
            Store::Disk(database) => {
                let Some(length) = header.values.get(slot).and_then(|value| value.length) else {
                    return Ok(None);
                };
                check()?;
                let bytes = self.observe(
                    database
                        .snapshot()
                        .and_then(|read| read.read_value(&address.value_id(slot).0, length))
                        .map_err(Error::from),
                )?;
                check()?;
                let data = self.observe((codec.decode)(&bytes))?;
                check()?;
                Ok(Some((
                    Value {
                        data,
                        codec: Some(codec),
                    },
                    true,
                )))
            }
        }
    }

    /// Recover the last validation revision without retaining a decoded query record.
    /// A session override hides the durable record's proof, even before it has been validated.
    pub(crate) fn verified(&self, id: &QueryId) -> u64 {
        if let Some(revision) = self.session.lock().expect("session storage").verified(id) {
            return revision;
        }
        self.usage
            .lock()
            .expect("storage usage")
            .get(id)
            .map_or(0, |entry| entry.verified)
    }

    /// Publish proofs only after their records have survived checkpointing and cancellation.
    /// Keeping the proof in the retention entry also removes it automatically with the record.
    pub(crate) fn remember_verified(&self, memos: &[Arc<Memo>]) {
        let mut usage = self.usage.lock().expect("storage usage");
        let mut session = self.session.lock().expect("session storage");
        for memo in memos {
            let state = memo.lock();
            let header = state.header.as_ref().expect("completed query header");
            match header.lifetime {
                Lifetime::Persistent => {
                    if let Some(entry) = usage.get_mut(&memo.id) {
                        entry.verified = state.verified;
                    }
                }
                Lifetime::Session => session.set_verified(&memo.id, state.verified),
                Lifetime::Operation => {
                    if let Some(entry) = usage.get_mut(&memo.id) {
                        entry.verified = 0;
                    }
                    session.set_verified(&memo.id, 0);
                }
            }
        }
    }

    pub(crate) fn is_disk(&self) -> bool {
        matches!(self.store, Store::Disk(_))
    }

    /// Refresh a saved result's retention age without writing to disk on every query read.
    /// If this key has never been saved, checkpointing will add its usage entry when it is retained.
    pub(crate) fn touch(&self, id: &QueryId, now: u64) {
        self.session.lock().expect("session storage").touch(id, now);
        if let Some(usage) = self.usage.lock().expect("storage usage").get_mut(id) {
            usage.used = now;
        }
    }

    /// A cheap size/count check for scheduling collection. Idle age is checked by the periodic
    /// collection pass instead of scanning all stored records after each operation.
    pub(crate) fn over_budget(&self, retention: Retention) -> bool {
        let usage = self.usage.lock().expect("storage usage");
        usage.len() > retention.max_entries
            || self.bytes.load(Ordering::Relaxed) > retention.max_bytes
    }

    /// Stored query count and logical encoded bytes for the database's statistics.
    /// This does not measure decoded working memory or physical storage size.
    pub(crate) fn usage(&self) -> (u64, u64) {
        (
            self.usage.lock().expect("storage usage").len() as u64,
            self.bytes.load(Ordering::Relaxed),
        )
    }

    /// Make a successful operation's new work available to later operations and disk restarts.
    ///
    /// Query execution has already made its answers usable in memory. Now save the memos marked
    /// dirty: encode changed payloads, remove outputs no longer emitted, and replace dependency
    /// records. Equal answers can still need a new record when they were computed from different
    /// reads. All disk changes share one transaction, so neither half an output set nor only part
    /// of this operation's dependent queries becomes durable.
    ///
    /// No query execution or equality comparison happens here. Workers have finished, and the
    /// memos already contain the new records and any payloads that need writing. Only after the
    /// write succeeds do we mark them saved and update retention accounting. Memory storage makes
    /// the same transition by keeping typed values rather than encoding them.
    pub(crate) fn checkpoint(
        &self,
        memos: &[Arc<Memo>],
        now: u64,
        parallelism: usize,
        stats: &mut Statistics,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        self.check()?;
        let dirty = memos
            .iter()
            .filter(|memo| memo.lock().dirty)
            .collect::<Vec<_>>();
        if dirty.is_empty() {
            return Ok(());
        }
        let _span = tracing::debug_span!("ajika.checkpoint", queries = dirty.len()).entered();

        // First prepare complete replacements. Session overrides do not modify their durable
        // baseline. In particular, an omitted output must hide an old slot without deleting it.
        let mut sessions = Vec::new();
        let mut durable = Vec::new();
        let mut operations = Vec::new();
        for memo in dirty {
            check()?;
            let (mut record, values) = {
                let state = memo.lock();
                (
                    state
                        .record
                        .as_deref()
                        .expect("completed query record")
                        .clone(),
                    state.values.clone(),
                )
            };
            record.last_used = now;
            if record.header.lifetime == Lifetime::Operation {
                operations.push((memo.clone(), Arc::new(record)));
                continue;
            }
            let mut retained = self
                .session
                .lock()
                .expect("session storage")
                .values(&memo.id, &record.header);
            retained.extend(values);
            let original = self.durable_record(&memo.id)?;
            if record.header.lifetime == Lifetime::Session {
                let stored =
                    SessionStore::prepare(record, retained, original.as_deref(), self.is_disk())?;
                sessions.push((memo.clone(), stored));
            } else {
                durable.push((memo.clone(), record, original, retained));
            }
        }

        // Dirty-only operations never open a disk writer. A save can promote a session value
        // directly: its typed value is already in retained, even if validation did not decode it.
        let result: Result<()> = (|| {
            let mut write = match &self.store {
                Store::Disk(database) if !durable.is_empty() => Some(database.begin_write()?),
                _ => None,
            };
            let assignments = write
                .as_ref()
                .map(|write| self.interner.write_pending(write))
                .transpose()?
                .unwrap_or_default();
            let mut completed = Vec::new();
            let mut pending = Vec::new();
            for (memo, mut record, original, values) in durable {
                check()?;
                if let Some(write) = &mut write {
                    if let Some(previous) = &original {
                        for (slot, value) in &previous.header.values {
                            if !record.header.values.contains_key(slot) && value.length.is_some() {
                                write.remove_value(&record.address.value_id(slot).0)?;
                            }
                        }
                    }
                    for (slot, output) in &mut Arc::make_mut(&mut record.header).values {
                        let backing = original
                            .as_ref()
                            .and_then(|old| old.header.values.get(slot));
                        if let Some(backing) = backing
                            && backing.version == output.version
                            && backing.length.is_some()
                        {
                            output.length = backing.length;
                            continue;
                        }
                        output.length = None;
                        if let Some(value) = values.get(slot)
                            && let Some(codec) = value.codec
                        {
                            let length = (codec.size)(&value.data)?;
                            output.length = Some(length);
                            pending.push(PendingValue {
                                id: record.address.value_id(slot),
                                value: value.clone(),
                                length,
                            });
                        }
                    }
                    record.bytes = record
                        .header
                        .values
                        .values()
                        .filter_map(|value| value.length)
                        .sum::<u64>()
                        + codec::encoded_size(&record)?;
                    let bytes = codec::encode(&record)?;
                    stats.metadata_writes += 1;
                    stats.metadata_bytes_written += bytes.len() as u64;
                    write.set_metadata(&memo.id.0, &bytes)?;
                } else {
                    // A promoted session record carries an encoded-size estimate. Typed memory
                    // storage uses only entry and age limits, so leave that estimate behind.
                    record.bytes = 0;
                }
                completed.push((memo, Arc::new(record), values));
            }
            if let Some(write) = &mut write {
                let Store::Disk(database) = &self.store else {
                    unreachable!()
                };
                stats.bytes_written += PendingValue::write_all(
                    &pending,
                    write,
                    database.compression(),
                    parallelism,
                    check,
                )?;
            }
            check()?;
            if let Some(write) = write {
                write.commit()?;
                self.interner.checkpointed(assignments);
            }

            // Publish only after the whole checkpoint succeeds. Failed operations can discard
            // their working memos and still recover the previous accepted session and disk state.
            let mut usage = self.usage.lock().expect("storage usage");
            let mut session = self.session.lock().expect("session storage");
            for (memo, record, values) in completed {
                if let Store::Memory(store) = &self.store {
                    let mut store = store.lock().expect("memory storage");
                    let mut retained = store
                        .get(&memo.id)
                        .map(|old| old.values.clone())
                        .unwrap_or_default();
                    retained.retain(|slot, _| record.header.values.contains_key(slot));
                    retained.extend(
                        values
                            .into_iter()
                            .filter(|(_, value)| value.codec.is_some()),
                    );
                    store.insert(
                        memo.id,
                        Stored {
                            record: record.clone(),
                            values: retained,
                        },
                    );
                }
                session.remove(&memo.id);
                let old = usage.insert(
                    memo.id,
                    Usage {
                        used: now,
                        bytes: record.bytes,
                        verified: 0,
                    },
                );
                self.bytes
                    .fetch_sub(old.map_or(0, |item| item.bytes), Ordering::Relaxed);
                self.bytes.fetch_add(record.bytes, Ordering::Relaxed);
                let mut state = memo.lock();
                state.install_record(record);
                state.dirty = false;
            }
            for (memo, stored) in sessions {
                let record = stored.record.clone();
                session.insert(memo.id, stored, now);
                let mut state = memo.lock();
                state.install_record(record);
                state.dirty = false;
            }
            for (memo, record) in operations {
                let mut state = memo.lock();
                state.install_record(record);
                state.dirty = false;
            }
            Ok(())
        })();
        self.observe(result)
    }

    pub(crate) fn session_usage(&self) -> (u64, u64) {
        self.session.lock().expect("session storage").usage()
    }

    pub(crate) fn collect_session(&self, retention: Retention) -> usize {
        self.session
            .lock()
            .expect("session storage")
            .collect(retention, Self::now())
    }

    pub(crate) fn clear_session(&self) {
        self.session.lock().expect("session storage").clear()
    }

    /// Remove the least recently used query results until storage is within its retention limits.
    /// A removed query loses its dependency record, output payloads and validity proof together.
    /// Return the number of removed records for the database's statistics.
    ///
    /// This is age/budget collection, not discovery of which queries a changed source made
    /// unreachable. A saved parent may still refer to a removed child. It keeps the child's full
    /// replay address, so the child can be computed again when requested, at the cost of lost reuse.
    /// This pass also saves accumulated last-use updates for records that remain.
    pub(crate) fn collect(&self, retention: Retention) -> Result<usize> {
        let _span = tracing::debug_span!("ajika.collect").entered();
        self.check()?;
        let now = Self::now();
        let max_idle = retention.max_idle.as_millis().min(u64::MAX as u128) as u64;
        let mut usage = self.usage.lock().expect("storage usage");
        if usage.is_empty() {
            return Ok(0);
        }
        // Choose whole queries using only the small usage inventory. Oldest records go first;
        // an expired record is removed even if the count and byte budgets already fit.
        let mut ordered = usage
            .iter()
            .map(|(id, usage)| (*id, *usage))
            .collect::<Vec<_>>();
        ordered.sort_by_key(|(id, usage)| (usage.used, *id));
        let mut bytes = ordered.iter().map(|(_, usage)| usage.bytes).sum::<u64>();
        let mut count = ordered.len();
        let mut removed = HashSet::new();
        for (id, item) in &ordered {
            if count > retention.max_entries
                || bytes > retention.max_bytes
                || now.saturating_sub(item.used) > max_idle
            {
                removed.insert(*id);
                count -= 1;
                bytes -= item.bytes;
            }
        }
        let result: Result<()> = (|| {
            match &self.store {
                Store::Memory(store) => {
                    let mut store = store.lock().expect("memory storage");
                    store.retain(|id, _| !removed.contains(id));
                    if store.len() < store.capacity() / 4 {
                        store.shrink_to_fit();
                    }
                }
                Store::Disk(database) => {
                    // Batch access-age bookkeeping with collection, rather than issuing
                    // a durable write after every warm read-only request.
                    // Read scopes pin their packs. Gather the small records first, then release
                    // the read guard before acquiring the exclusive storage writer.
                    let records = {
                        let read = database.snapshot()?;
                        ordered
                            .iter()
                            .map(|(id, item)| Ok((*id, *item, read.metadata(&id.0)?)))
                            .collect::<Result<Vec<_>>>()?
                    };
                    let mut write = database.begin_write()?;
                    for (id, item, bytes) in &records {
                        let Some(bytes) = bytes else {
                            continue;
                        };
                        let mut record: Record = codec::decode(bytes)?;
                        if removed.contains(id) {
                            for (slot, value) in &record.header.values {
                                if value.length.is_some() {
                                    write.remove_value(&record.address.value_id(slot).0)?;
                                }
                            }
                            write.remove_metadata(&id.0)?;
                        } else if record.last_used != item.used {
                            record.last_used = item.used;
                            write.set_metadata(&id.0, &codec::encode(&record)?)?;
                        }
                    }
                    write.commit()?;
                }
            }
            Ok(())
        })();
        self.observe(result)?;
        // Stored removals succeeded. A failed pass must not advance this bookkeeping as if
        // its removals had been saved, so update the in-memory inventory only afterward.
        {
            let mut session = self.session.lock().expect("session storage");
            for id in &removed {
                session.remove(id);
            }
        }
        usage.retain(|id, _| !removed.contains(id));
        if usage.len() < usage.capacity() / 4 {
            usage.shrink_to_fit();
        }
        self.bytes.store(bytes, Ordering::Relaxed);
        Ok(removed.len())
    }

    /// Discard retained query work. A healthy index can keep its saved identity table.
    /// When the index needs repair, rebuild it from the live assignments so the same IDs
    /// are available on restart.
    pub(crate) fn invalidate(&mut self) -> Result<()> {
        let result = match &mut self.store {
            Store::Memory(store) => {
                *store.get_mut().expect("memory storage") = HashMap::new();
                Ok(())
            }
            Store::Disk(database) if self.recovery.get().is_some() => {
                self.interner.rebuild(database)
            }
            Store::Disk(database) => match database.invalidate() {
                Err(error) if error.kind() == ajika_storage::ErrorKind::InvalidCache => {
                    self.interner.rebuild(database)
                }
                result => result.map_err(Error::from),
            },
        };
        self.observe(result)?;
        self.recovery.take();
        self.replace_usage(HashMap::new());
        Ok(())
    }

    /// Ask disk storage to reclaim physical file space after logical removals. Memory storage
    /// has no file to compact, and simply reports that no compaction happened.
    pub(crate) fn compact(&mut self) -> Result<bool> {
        self.check()?;
        let result = match &mut self.store {
            Store::Memory(_) => Ok(false),
            Store::Disk(database) => database.compact().map_err(Error::from),
        };
        self.observe(result)
    }

    /// Storage relocation changes no logical query state. Run it outside the evaluator's
    /// working scope so there are no disk readers or query workers to coordinate with.
    pub(crate) fn maintain(&self) -> Result<()> {
        self.check()?;
        match &self.store {
            Store::Memory(_) => Ok(()),
            Store::Disk(database) => self.observe(database.maintain().map_err(Error::from)),
        }
    }
}
