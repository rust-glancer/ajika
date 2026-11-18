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
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ajika_storage::Database as StorageDatabase;

use super::{
    cache::Memo,
    record::{Address, QueryId, Record, RecordHeader, Slot, ValueId},
    value::{Codec, Value},
};
use crate::{DatabaseOptions, Error, Lifetime, Result, Retention, Statistics, codec};

mod encoding;
mod resident;
use self::{encoding::PendingValue, resident::ResidentStore};

/// The record describes every output. The map holds the typed values this entry owns.
/// A session can leave an equal output with the persistent store. A pending write can
/// leave an equal output on disk.
struct Stored {
    record: Arc<Record>,
    values: BTreeMap<Slot, Value>,
}

impl Stored {
    /// An edit can change one output and leave the others equal to the saved version.
    /// Keep the edited outputs here. For equal outputs, copy the persistent record's
    /// length so the session can still read them after a pending write reaches disk.
    fn session(
        mut record: Record,
        mut values: BTreeMap<Slot, Value>,
        baseline: Option<&Record>,
        disk: bool,
    ) -> Result<Self> {
        for (slot, output) in &mut Arc::make_mut(&mut record.header).values {
            let backing = baseline.and_then(|old| old.header.values.get(slot));
            if let Some(backing) = backing
                && backing.version == output.version
                && (!disk || backing.length.is_some())
            {
                output.length = backing.length;
                values.remove(slot);
            } else {
                output.length = None;
            }
        }
        values.retain(|_, value| value.codec.is_some());
        record.bytes = codec::encoded_size(&record)?;
        for value in values.values() {
            record.bytes += (value.codec.expect("retained payload codec").size)(&value.data)?;
        }
        Ok(Self {
            record: Arc::new(record),
            values,
        })
    }
}

/// Small process-local facts about a stored record. Retention needs its size and last use;
/// reuse needs the input revision in which it was checked. Keeping them together means a
/// collected or replaced record cannot leave an unrelated validity proof behind.
#[derive(Clone, Copy)]
struct Usage {
    used: u64,
    // Warm reads defer timestamp writes until collection.
    used_dirty: bool,
    bytes: u64,
    // This proof belongs to the stored record, not a decoded working copy. Reopening starts
    // at zero; only a successful operation can establish validity for this database's inputs.
    verified: u64,
}

enum Store {
    Memory(Mutex<HashMap<QueryId, Stored>>),
    Disk(Box<StorageDatabase>),
}

/// One operation can update both session and persistent results. If it needs a disk
/// write, that write must succeed before any of these results become visible to later runs.
struct PreparedCheckpoint {
    queries: Vec<(QueryId, Stored)>,
    disk: DiskChanges,
}

/// Disk mutations are decided before opening a writer. Records and payloads stay typed until
/// writing, so preparation does not duplicate the whole dependency graph as encoded bytes.
#[derive(Default)]
struct DiskChanges {
    records: Vec<MetadataWrite>,
    removed_values: Vec<ValueId>,
    payloads: Vec<PendingValue>,
}

impl DiskChanges {
    /// Plan the writes and deletions for one query. Suppose its disk record lists
    /// outputs A and B. Several runs later only A remains. We need the record from
    /// disk to discover that B's old payload still needs deleting.
    fn replace(
        &mut self,
        id: QueryId,
        mut record: Record,
        retained: &BTreeMap<Slot, Value>,
        original: Option<&Record>,
    ) -> Result<Arc<Record>> {
        if let Some(previous) = original {
            for (slot, value) in &previous.header.values {
                if !record.header.values.contains_key(slot) && value.length.is_some() {
                    self.removed_values.push(record.address.value_id(slot));
                }
            }
        }
        // A matching version can reuse the bytes already on disk. For a new value,
        // record its encoded length now so readers will know how many bytes to load.
        for (slot, output) in &mut Arc::make_mut(&mut record.header).values {
            let backing = original.and_then(|old| old.header.values.get(slot));
            if let Some(backing) = backing
                && backing.version == output.version
                && backing.length.is_some()
            {
                output.length = backing.length;
                continue;
            }
            if let Some(value) = retained.get(slot)
                && let Some(codec) = value.codec
            {
                // Deferred checkpoints measured this value when accepting the run.
                let length = match output.length {
                    Some(length) => length,
                    None => (codec.size)(&value.data)?,
                };
                output.length = Some(length);
                self.payloads.push(PendingValue {
                    id: record.address.value_id(slot),
                    value: value.clone(),
                    length,
                });
            } else {
                output.length = None;
            }
        }
        record.bytes = record
            .header
            .values
            .values()
            .filter_map(|value| value.length)
            .sum::<u64>()
            + codec::encoded_size(&record)?;
        let record = Arc::new(record);
        self.records.push(MetadataWrite {
            id,
            record: record.clone(),
        });
        Ok(record)
    }
}

struct MetadataWrite {
    id: QueryId,
    record: Arc<Record>,
}

struct Deferred {
    // Later runs can replace a query's pending record. Only the latest one needs writing.
    entries: Mutex<ResidentStore>,
    max_bytes: u64,
}

impl Deferred {
    /// Prepare the next persistent version of this query. Its header can be used to
    /// read the same outputs both before and after the write.
    fn prepare(
        &self,
        id: &QueryId,
        mut record: Record,
        mut values: BTreeMap<Slot, Value>,
    ) -> Result<Stored> {
        // A known length belongs to the persistent version that compared equal.
        // Discard any newly computed copy, which may even encode to a different size.
        values.retain(|slot, value| {
            value.codec.is_some()
                && record
                    .header
                    .values
                    .get(slot)
                    .is_some_and(|output| output.length.is_none())
        });
        // If that version is still waiting to be written, carry its original Rust
        // value into the replacement. Otherwise its bytes are already on disk.
        self.entries
            .lock()
            .expect("pending checkpoints")
            .fill_values(id, &record.header, &mut values);

        let mut bytes = 0;
        let header = Arc::make_mut(&mut record.header);
        for (slot, value) in &values {
            let output = header.values.get_mut(slot).expect("retained output");
            let length = match output.length {
                Some(length) => length,
                None => {
                    let codec = value.codec.expect("retained payload codec");
                    let length = (codec.size)(&value.data)?;
                    output.length = Some(length);
                    length
                }
            };
            bytes += length;
        }
        record.bytes = bytes + codec::encoded_size(&record)?;
        Ok(Stored {
            record: Arc::new(record),
            values,
        })
    }
}

/// Keeps query results for reuse after a run. During the run, queries share new work
/// through their memos. The callback can still fail, so we wait until checkpointing to
/// install that work here.
///
/// To find a previous result, look in the session store first, then among persistent
/// results waiting for a write, then in the database. For example, the session can hold
/// a result for an unsaved editor buffer while the result for the saved file waits for disk.
pub(crate) struct Backend {
    pub(crate) interner: crate::Interner,
    session: Mutex<ResidentStore>,
    deferred: Option<Deferred>,
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
            deferred: None,
            session: Mutex::default(),
            usage: Mutex::default(),
            recovery: OnceLock::new(),
            bytes: AtomicU64::new(0),
        }
    }

    /// Open saved work and rebuild its retention inventory. This reads record metadata, not
    /// query payloads; memos and validity proofs are established later as queries are requested.
    pub(crate) fn open(path: &Path, epoch: &str, options: &DatabaseOptions) -> Result<Self> {
        // The runtime's record schema has its own version, separate from the storage
        // adapter and the application's persisted query types.
        let mut database =
            StorageDatabase::open(path, &format!("query-runtime-3:{epoch}"), options.storage)?;
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
            store: Store::Disk(Box::new(database)),
            deferred: options.max_pending_bytes.map(|max_bytes| Deferred {
                entries: Mutex::default(),
                max_bytes,
            }),
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
                    used_dirty: false,
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
        // Both session and pending records can refer to outputs in the old disk cache.
        // After recovery or invalidation those references may point to missing or different
        // values. Start both maps empty and validate any recovered records on demand.
        self.session.get_mut().expect("session storage").clear();
        if let Some(mut pending) = self.pending() {
            pending.clear();
        }
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

    fn pending(&self) -> Option<MutexGuard<'_, ResidentStore>> {
        self.deferred
            .as_ref()
            .map(|deferred| deferred.entries.lock().expect("pending checkpoints"))
    }

    pub(crate) fn is_deferred(&self) -> bool {
        self.deferred.is_some()
    }

    pub(crate) fn pending_over_budget(&self) -> bool {
        self.deferred.as_ref().is_some_and(|deferred| {
            deferred
                .entries
                .lock()
                .expect("pending checkpoints")
                .usage()
                .1
                > deferred.max_bytes
        })
    }

    pub(crate) fn pending_usage(&self) -> (u64, u64) {
        self.pending().map_or((0, 0), |pending| pending.usage())
    }

    /// Load only the output metadata into a known-valid call's working memo. Disk storage
    /// still reads the complete metadata row, but decoding stops before dependency addresses.
    /// This keeps one stored record without allocating a dependency graph just to load a value.
    pub(crate) fn header(&self, id: &QueryId) -> Result<(Option<Arc<RecordHeader>>, bool)> {
        self.check()?;
        let resident = self
            .session
            .lock()
            .expect("session storage")
            .record(id)
            .or_else(|| self.pending()?.record(id));
        if let Some(record) = resident {
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
        self.persistent_record(id)
    }

    /// An unsaved edit can hide the saved result of the same query. Ask for the
    /// persistent record directly when choosing which outputs the edit can reuse.
    fn persistent_record(&self, id: &QueryId) -> Result<(Option<Arc<Record>>, bool)> {
        if let Some(record) = self.pending().and_then(|pending| pending.record(id)) {
            return Ok((Some(record), false));
        }
        let record = match &self.store {
            Store::Memory(store) => store
                .lock()
                .expect("memory storage")
                .get(id)
                .map(|stored| stored.record.clone()),
            Store::Disk(database) => self.observe((|| {
                database
                    .snapshot()?
                    .metadata(&id.0)?
                    .map(|bytes| codec::decode(&bytes).map(Arc::new))
                    .transpose()
            })())?,
        };
        Ok((record, self.is_disk()))
    }

    /// Load the output version named in `header`. A session record can describe an edited
    /// file but reuse one output from the saved file. Look for that exact version in RAM,
    /// then use its disk length if we have one. If no value was retained, the evaluator
    /// decides whether to run the query. The flag tells it whether to count a disk read.
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
        let id = address.id();
        if let Some(value) = self
            .session
            .lock()
            .expect("session storage")
            .value(&id, header, slot)
            .or_else(|| self.pending()?.value(&id, header, slot))
        {
            return Ok(Some((value, false)));
        }
        match &self.store {
            Store::Memory(store) => Ok(store
                .lock()
                .expect("memory storage")
                .get(&id)
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

    /// Use the revision from the same entry that supplies the record. A result for an
    /// edited file cannot inherit the saved file's validation just because the query key matches.
    pub(crate) fn verified(&self, id: &QueryId) -> u64 {
        if let Some(revision) = self
            .session
            .lock()
            .expect("session storage")
            .verified(id)
            .or_else(|| self.pending()?.verified(id))
        {
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
        let mut pending = self.pending();
        for memo in memos {
            let state = memo.lock();
            let header = state.header.as_ref().expect("completed query header");
            match header.lifetime {
                Lifetime::Persistent => {
                    if let Some(pending) = &mut pending
                        && pending.set_verified(&memo.id, state.verified)
                    {
                        continue;
                    }
                    if let Some(entry) = usage.get_mut(&memo.id) {
                        entry.verified = state.verified;
                    }
                }
                Lifetime::Session => {
                    session.set_verified(&memo.id, state.verified);
                }
                Lifetime::Operation => {
                    if let Some(entry) = usage.get_mut(&memo.id) {
                        entry.verified = 0;
                    }
                    session.set_verified(&memo.id, 0);
                    if let Some(pending) = &mut pending {
                        pending.set_verified(&memo.id, 0);
                    }
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
        if let Some(mut pending) = self.pending() {
            pending.touch(id, now);
        }
        if let Some(usage) = self.usage.lock().expect("storage usage").get_mut(id)
            && usage.used != now
        {
            usage.used = now;
            usage.used_dirty = true;
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

    /// Make a successful run's changes available to the next run. Each changed query
    /// supplies a whole record. Even a query that returned the same value may have read
    /// different inputs or stopped emitting a named output.
    ///
    /// Prepare all replacements before installing any of them. For an eager disk write,
    /// the transaction must also commit. With deferred writes, installing the persistent
    /// results in memory is enough for the next run to see them.
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

        let prepared = self.prepare_checkpoint(&dirty, now, check)?;
        self.write_checkpoint(&prepared.disk, parallelism, stats, check)?;
        self.publish_checkpoint(prepared, now);
        Ok(())
    }

    /// The query's lifetime decides what survives this run. Its outputs can have
    /// different storage needs, even though they all belong to the same record.
    fn prepare_checkpoint(
        &self,
        dirty: &[&Arc<Memo>],
        now: u64,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<PreparedCheckpoint> {
        let mut prepared = Vec::with_capacity(dirty.len());
        let mut disk = DiskChanges::default();
        for &memo in dirty {
            check()?;
            // Evaluation is over. Move the record and its values out of the memo while
            // we still own them, so finishing the request doesn't require another copy.
            let (record, mut retained) = {
                let mut state = memo.lock();
                let record = state.record.take().expect("completed query record");
                if record.header.lifetime == Lifetime::Operation {
                    continue;
                }
                (record, std::mem::take(&mut state.values))
            };
            let mut record = Arc::unwrap_or_clone(record);
            record.last_used = now;
            // Saving the exact text that's open in an editor can change a query's lifetime
            // without running it again. Its memo may be missing values that now need to be
            // saved. Pick up the edited values before replacing the session entry.
            self.session.lock().expect("session storage").fill_values(
                &memo.id,
                &record.header,
                &mut retained,
            );
            let stored = match (record.header.lifetime, &self.store) {
                (Lifetime::Operation, _) => unreachable!("operation work was released above"),
                (Lifetime::Session, _) => {
                    let (original, _) = self.persistent_record(&memo.id)?;
                    Stored::session(record, retained, original.as_deref(), self.is_disk())?
                }
                (Lifetime::Persistent, Store::Memory(store)) => {
                    // Keep the complete typed replacement, including unloaded values from
                    // the old entry. Memory storage does not charge encoded disk bytes.
                    record.bytes = 0;
                    let mut memory_values = store
                        .lock()
                        .expect("memory storage")
                        .get(&memo.id)
                        .map(|old| old.values.clone())
                        .unwrap_or_default();
                    memory_values.retain(|slot, _| record.header.values.contains_key(slot));
                    memory_values.extend(
                        retained
                            .into_iter()
                            .filter(|(_, value)| value.codec.is_some()),
                    );
                    Stored {
                        record: Arc::new(record),
                        values: memory_values,
                    }
                }
                (Lifetime::Persistent, Store::Disk(_)) => {
                    if let Some(deferred) = &self.deferred {
                        deferred.prepare(&memo.id, record, retained)?
                    } else {
                        let (original, _) = self.persistent_record(&memo.id)?;
                        let record =
                            disk.replace(memo.id, record, &retained, original.as_deref())?;
                        Stored {
                            record,
                            values: BTreeMap::new(),
                        }
                    }
                }
            };
            prepared.push((memo.id, stored));
        }
        Ok(PreparedCheckpoint {
            queries: prepared,
            disk,
        })
    }

    /// Commit the query records, payloads and any new IDs they reference together.
    /// If this checkpoint only changes what's in memory, we can skip the disk writer.
    fn write_checkpoint(
        &self,
        disk: &DiskChanges,
        parallelism: usize,
        stats: &mut Statistics,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        let written: Result<()> = (|| {
            check()?;
            if let Store::Disk(database) = &self.store
                && !disk.records.is_empty()
            {
                let mut write = database.begin_write()?;
                let assignments = self.interner.write_pending(&write)?;
                for id in &disk.removed_values {
                    check()?;
                    write.remove_value(&id.0)?;
                }
                for record in &disk.records {
                    check()?;
                    // Release each encoded record after the writer copies it. Retaining the
                    // whole batch would keep a second copy of the dependency graph alive while
                    // payloads are encoded. Publication still waits for the transaction to commit.
                    let bytes = codec::encode(record.record.as_ref())?;
                    stats.metadata_writes += 1;
                    stats.metadata_bytes_written += bytes.len() as u64;
                    write.set_metadata(&record.id.0, &bytes)?;
                }
                stats.bytes_written += PendingValue::write_all(
                    &disk.payloads,
                    &mut write,
                    database.compression(),
                    parallelism,
                    check,
                )?;
                check()?;
                write.commit()?;
                self.interner.checkpointed(assignments);
            }
            Ok(())
        })();
        self.observe(written)
    }

    /// Install the results that later queries will see. When an editor saves a file,
    /// remove the old session entry so it can't hide the new persistent result, even
    /// if that result is still waiting for disk.
    fn publish_checkpoint(&self, prepared: PreparedCheckpoint, now: u64) {
        let mut usage = self.usage.lock().expect("storage usage");
        let mut session = self.session.lock().expect("session storage");
        let mut pending = self.pending();
        for (id, stored) in prepared.queries {
            if stored.record.header.lifetime == Lifetime::Session {
                session.insert(id, stored, now);
                continue;
            }
            session.remove(&id);
            if let Some(pending) = &mut pending {
                pending.insert(id, stored, now);
                continue;
            }

            let bytes = stored.record.bytes;
            if let Store::Memory(store) = &self.store {
                store.lock().expect("memory storage").insert(id, stored);
            }
            let old = usage.insert(
                id,
                Usage {
                    used: now,
                    used_dirty: false,
                    bytes,
                    verified: 0,
                },
            );
            self.bytes
                .fetch_sub(old.map_or(0, |item| item.bytes), Ordering::Relaxed);
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Write the latest pending result for each query in one transaction. These results
    /// are already serving reads. Keep them available until the write commits, so a
    /// rolled-back write can be retried. If the commit outcome is uncertain, recovery
    /// clears results in RAM that might point to obsolete disk data.
    pub(crate) fn drain_pending(
        &mut self,
        parallelism: usize,
        stats: &mut Statistics,
    ) -> Result<()> {
        self.check()?;
        let Some(deferred) = &mut self.deferred else {
            return Ok(());
        };
        let pending = deferred.entries.get_mut().expect("pending checkpoints");
        let (count, bytes) = pending.usage();
        if count == 0 {
            return Ok(());
        }
        let _span = tracing::debug_span!("ajika.drain_pending", queries = count, bytes).entered();
        let Store::Disk(database) = &self.store else {
            unreachable!("only disk databases defer checkpoints")
        };
        let usage = self.usage.get_mut().expect("storage usage");
        let mut disk = DiskChanges::default();
        // Read the old records in one snapshot. If a query has no entry in `usage`,
        // there's nothing on disk to compare or delete for that query.
        let prepared = (|| {
            let read = database.snapshot()?;
            for (id, stored, used) in pending.entries() {
                let original = if usage.contains_key(&id) {
                    read.metadata(&id.0)?
                        .map(|bytes| codec::decode(&bytes))
                        .transpose()?
                } else {
                    None
                };
                let mut record = stored.record.as_ref().clone();
                record.last_used = used;
                disk.replace(id, record, &stored.values, original.as_ref())?;
            }
            Ok(())
        })();
        self.observe(prepared)?;
        // These results belong to runs that already finished. Complete their write
        // even if the request that triggered it has since been cancelled.
        self.write_checkpoint(&disk, parallelism, stats, &|| Ok(()))?;
        // Readers can now load the committed versions. Release their in-memory copies
        // and carry each query's last use and validation revision over to the disk entry.
        let pending = self
            .deferred
            .as_mut()
            .expect("draining deferred work")
            .entries
            .get_mut()
            .expect("pending checkpoints");
        let usage = self.usage.get_mut().expect("storage usage");
        for MetadataWrite { id, record } in disk.records {
            let replacement = Usage {
                bytes: record.bytes,
                used: record.last_used,
                used_dirty: false,
                verified: pending.verified(&id).expect("drained record is pending"),
            };
            let old = usage.insert(id, replacement);
            self.bytes
                .fetch_sub(old.map_or(0, |item| item.bytes), Ordering::Relaxed);
            self.bytes.fetch_add(record.bytes, Ordering::Relaxed);
        }
        pending.clear();
        Ok(())
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
        // Disk space can run out before the pending results are written. Choose what
        // to keep from both sets. If a query is in both, its pending record describes
        // what we'd keep, including any unchanged outputs already on disk.
        let mut ordered = Vec::with_capacity(usage.len());
        {
            let pending = self.pending();
            for (id, entry) in usage.iter() {
                if !pending.as_ref().is_some_and(|pending| pending.contains(id)) {
                    ordered.push((*id, entry.used, entry.bytes));
                }
            }
            if let Some(pending) = pending {
                for (id, stored, used) in pending.entries() {
                    let backed_bytes: u64 = stored
                        .record
                        .header
                        .values
                        .iter()
                        .filter(|(slot, _)| !stored.values.contains_key(*slot))
                        .filter_map(|(_, output)| output.length)
                        .sum();
                    ordered.push((id, used, stored.record.bytes + backed_bytes));
                }
            }
        }
        if ordered.is_empty() {
            return Ok(0);
        }
        ordered.sort_by_key(|(id, used, _)| (*used, *id));
        let mut bytes = ordered.iter().map(|(_, _, bytes)| *bytes).sum::<u64>();
        let mut count = ordered.len();
        let mut removed = HashSet::new();
        for (id, used, size) in ordered {
            if count > retention.max_entries
                || bytes > retention.max_bytes
                || now.saturating_sub(used) > max_idle
            {
                removed.insert(id);
                count -= 1;
                bytes -= size;
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
                    // Evicting a query that has only ever lived in RAM needs no disk write.
                    let mut changes = usage
                        .iter()
                        .filter(|(id, item)| removed.contains(*id) || item.used_dirty)
                        .peekable();
                    if changes.peek().is_some() {
                        let mut write = database.begin_write()?;
                        // Read through the writer so each encoded record can be released before
                        // loading the next. A separate snapshot would prevent exclusive writes.
                        for (id, item) in changes {
                            let Some(bytes) = write.metadata(&id.0)? else {
                                continue;
                            };
                            let mut record: Record = codec::decode(&bytes)?;
                            if removed.contains(id) {
                                for (slot, value) in &record.header.values {
                                    if value.length.is_some() {
                                        write.remove_value(&record.address.value_id(slot).0)?;
                                    }
                                }
                                write.remove_metadata(&id.0)?;
                            } else {
                                record.last_used = item.used;
                                write.set_metadata(&id.0, &codec::encode(&record)?)?;
                            }
                        }
                        write.commit()?;
                    }
                }
            }
            Ok(())
        })();
        self.observe(result)?;
        // Removals and timestamp updates succeeded. A failed pass must keep dirty timestamps
        // for retry and must not forget records whose removal was rolled back.
        {
            let mut session = self.session.lock().expect("session storage");
            let mut pending = self.pending();
            for id in &removed {
                // Both in-memory entries may still refer to payloads we just deleted.
                session.remove(id);
                if let Some(pending) = &mut pending {
                    pending.remove(id);
                }
            }
        }
        let mut stored_bytes = 0;
        usage.retain(|id, item| {
            if removed.contains(id) {
                return false;
            }
            item.used_dirty = false;
            stored_bytes += item.bytes;
            true
        });
        if usage.len() < usage.capacity() / 4 {
            usage.shrink_to_fit();
        }
        self.bytes.store(stored_bytes, Ordering::Relaxed);
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
