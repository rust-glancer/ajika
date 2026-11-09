//! Serving input, query, and named-output requests made through [`Db`].
//!
//! This is where an ordinary query call becomes a decision to reuse an answer, load one from
//! storage, or execute the query function. It also connects reads to the calling query's frame
//! so future calls know what to check. Replaying saved dependencies uses these same evaluation
//! paths; it does not have a second implementation of query execution or validity checking.

use std::{collections::BTreeMap, sync::Arc};

use super::{
    Db, Output, OutputSlot,
    cache::Memo,
    diagnostics::{self, OutputOutcome, ReuseReason},
    frame::Frame,
    record::{Address, Dependency, Record, RecordHeader, Slot, ValueRecord},
    value::{Codec, Value},
};
use crate::{
    Durability, Error, Input, Lifetime, Persist, Query, Result, UnavailableData, error::ensure,
};

impl<'db, S: Send + Sync + 'static> Db<'db, S> {
    /// Ask query Q for the answer to `key` under this operation's captured inputs.
    /// Repeated calls can share a loaded answer; later calls may load a saved answer or compute
    /// a new one. The caller always receives an ordinary [`Arc`] and need not choose the path.
    ///
    /// Inside another query, this also records which answer the parent used. For example,
    /// `word_count` depends on the version of the answer from `words`, so an edit that leaves the words
    /// equal need not rerun `word_count`.
    ///
    /// A free `#[ajika::query]` function generates a type with the same name. For example,
    /// `db.query::<words>(file)?` obtains an owned answer even if `words` uses `returns(ref)`.
    /// Pass `()` for no key arguments, the argument itself for one, or a tuple for several.
    /// For a handwritten query, use its implementing type and pass its [`Key`](Query::Key).
    pub fn query<Q: Query<Services = S>>(&self, key: Q::Key) -> Result<Arc<Q::Value>> {
        self.handle(|| {
            self.check_cancelled()?;
            if let Some(value) = self.frame.query::<Q>(&key) {
                // This fast path normally avoids encoding the key. Only diagnostics need its
                // address, and a diagnostic failure must not turn a successful read into an error.
                if tracing::enabled!(target: diagnostics::TARGET, tracing::Level::TRACE)
                    && let Ok(address) = Address::new(Q::NAME, &key)
                {
                    self.operation
                        .diagnostic_reuse(&address, &None, ReuseReason::ResidentValue);
                }
                return Ok(value);
            }
            let memo = self.ensure::<Q>(key.clone(), true)?;
            let state = memo.lock();
            let header = state.header.as_ref().expect("verified query header");
            // The caller depends on this answer's version, not on whether its provider ran.
            // The child's own input observations stay in the child's record.
            self.frame.depend(
                Dependency::Value(memo.address.clone(), None, header.values[&None].version),
                header.durability,
                header.lifetime,
            );

            let value = state
                .values
                .get(&None)
                .expect("requested query value is resident")
                .typed()?;

            Ok(self.frame.remember_query::<Q>(key, value))
        })
    }

    /// Borrow a query answer for the lifetime of this context.
    /// Use the same query type and key as [`query`](Self::query), as in
    /// `db.query_ref::<words>(file)?`. Both calls record the same dependency.
    /// Use `query` to keep the answer after the operation ends.
    pub fn query_ref<Q: Query<Services = S>>(&self, key: Q::Key) -> Result<&'db Q::Value> {
        let value = self.query::<Q>(key)?;
        // SAFETY: query returns the allocation kept in the frame. The frame never
        // removes or replaces acquired query values and is borrowed for 'db.
        Ok(unsafe { &*Arc::as_ptr(&value) })
    }

    /// Read one typed input field and record a dependency when called from a query.
    /// Reads within a snapshot reuse the captured value for that field and key.
    pub fn input<T: Input>(&self, key: T::Key) -> Result<T::Value> {
        self.handle(|| {
            self.check_cancelled()?;
            let captured = self.operation.database.inputs.read::<T>(&key)?;
            self.frame.depend(
                Dependency::Input(Address::new(T::NAME, &key)?, captured.record.stamp),
                captured.record.durability,
                captured.record.lifetime,
            );
            self.check_cancelled()?;
            Ok(captured.value)
        })
    }

    /// Give this query an additional result that callers can read and depend on separately.
    /// For example, one computation can emit `"shape"` and `"locations"`, then return a set
    /// of output handles. A caller reading only shape need not load locations,
    /// and changing locations alone need not invalidate that caller.
    ///
    /// Use a fixed name or `("name", key)` for repeated outputs, such as one body per function.
    /// Each slot belongs to the executing query and may be emitted once. The query's ordinary
    /// return value and all named outputs become available together after execution succeeds.
    /// Until then, use local values inside the producer; its own output handles cannot be read yet.
    pub fn output<T: Persist + PartialEq + Send + Sync + 'static>(
        &self,
        slot: impl OutputSlot,
        value: Arc<T>,
    ) -> Result<Output<T>> {
        self.handle(|| {
            self.check_cancelled()?;
            let frame = self.frame;
            let producer = frame
                .owner
                .clone()
                .ok_or_else(|| Error::contract("outputs require an executing query"))?;
            let slot = slot.encode()?;
            let value = Value {
                data: value,
                codec: Some(Codec::of::<T>()),
            };
            let mut reads = frame.reads.lock().expect("query outputs");
            ensure!(
                reads.outputs.insert(Some(slot.clone()), value).is_none(),
                Error::contract("query emitted an output slot twice"),
            );
            Ok(Output::new(producer, slot))
        })
    }

    /// Serve [`Output::value`]: establish that the query which produced the output is valid,
    /// obtain this particular value, and record its version in the caller's frame.
    /// The selector already identifies the query and slot, so there is no need to load its
    /// ordinary return value first or depend on versions of sibling outputs.
    /// The frame retains the returned allocation for as long as the frame can be borrowed.
    pub(crate) fn output_value<T: Persist + PartialEq + Send + Sync + 'static>(
        &self,
        address: &Address,
        slot: &[u8],
    ) -> Result<Arc<T>> {
        self.handle(|| {
            self.check_cancelled()?;
            ensure!(
                self.frame.owner.as_ref() != Some(address),
                Error::contract("query cannot read its own output while executing"),
            );
            if let Some(value) = self.frame.acquired(address, slot)? {
                if tracing::enabled!(target: diagnostics::TARGET, tracing::Level::TRACE) {
                    self.operation.diagnostic_reuse(
                        address,
                        &Some(slot.to_vec()),
                        ReuseReason::ResidentValue,
                    );
                }
                return Ok(value);
            }
            let registry = &self.operation.database.registry;
            let memo =
                registry
                    .replay(self, address, false)?
                    .ok_or_else(|| UnavailableData::Output {
                        producer: address.family.clone(),
                        slot: slot.to_vec(),
                    })?;
            let slot = Some(slot.to_vec());
            let mut value = self.materialize_output::<T>(&memo, &slot)?;
            if value.is_none() {
                // A producer may have reusable metadata but no retained output payload.
                // Asking for its value reruns the same provider.
                registry.replay(self, address, true)?;
                value = self.materialize_output::<T>(&memo, &slot)?;
            }
            let value = value.ok_or_else(|| UnavailableData::Output {
                producer: address.family.clone(),
                slot: slot.clone().unwrap_or_default(),
            })?;
            let state = memo.lock();
            let header = state.header.as_ref().expect("output producer is verified");
            self.frame.depend(
                Dependency::Value(address.clone(), slot.clone(), header.values[&slot].version),
                header.durability,
                header.lifetime,
            );
            self.frame
                .remember(address, slot.as_ref().expect("named output"), value)
        })
    }

    /// Obtain a named value from an already verified query, sharing a decoded copy if available.
    /// Otherwise load it under the query's claim so workers do not decode it simultaneously.
    /// None means the known output has no retained payload and the caller must reconstruct it.
    /// If the verified query no longer emits this slot, return an error.
    fn materialize_output<T: Persist + PartialEq + Send + Sync + 'static>(
        &self,
        memo: &Memo,
        slot: &Slot,
    ) -> Result<Option<Arc<T>>> {
        if let Some(value) = memo.lock().values.get(slot) {
            return Ok(Some(value.typed()?));
        }
        self.claimed(memo, || {
            let header = {
                let state = memo.lock();
                if let Some(value) = state.values.get(slot) {
                    return Ok(Some(value.typed()?));
                }
                state.header.clone().expect("output producer is verified")
            };
            if !header.values.contains_key(slot) {
                return Err(UnavailableData::Output {
                    producer: memo.address.family.clone(),
                    slot: slot.clone().unwrap_or_default(),
                }
                .into());
            }
            let Some(value) = self.load(&memo.address, &header, slot, Codec::of::<T>(), false)?
            else {
                return Ok(None);
            };
            let result = value.typed()?;
            memo.lock().values.insert(slot.clone(), value);
            Ok(Some(result))
        })
    }

    /// Establish that this query's answer is valid for the current inputs, and optionally load it.
    ///
    /// First look for a memo already checked in this revision. If that does not settle validity,
    /// use durability or check its saved dependencies. A missing record or changed dependency
    /// requires execution to establish an answer. Once valid, load the value if requested;
    /// a query with a transient result may need execution just to reconstruct that missing value.
    ///
    /// With `want_value = false`, callers only need the answer's current version. This is enough
    /// to validate a parent's dependency without loading every intermediate result. It can still
    /// execute an invalidated child. With true, the returned memo also holds the ordinary return value.
    pub(crate) fn ensure<Q: Query<Services = S>>(
        &self,
        key: Q::Key,
        want_value: bool,
    ) -> Result<Arc<Memo>> {
        self.check_cancelled()?;
        let memo = self.operation.memos.get::<Q>(&key)?;
        self.operation.touch(&memo);
        let ready = || {
            let state = memo.lock();
            state.verified == self.operation.database.revision
                && state.header.is_some()
                && (!want_value || state.values.contains_key(&None))
        };
        if ready() {
            {
                let mut statistics = self.operation.stats();
                statistics.hits += 1;
                statistics.resident_hits += u64::from(want_value);
            }
            self.operation.diagnostic_reuse(
                &memo.address,
                &None,
                if want_value {
                    ReuseReason::ResidentValue
                } else {
                    ReuseReason::ValidityProof
                },
            );
            return Ok(memo);
        }

        // The fast path needs no claim. Once it misses, reserve this key through validation,
        // decoding, or computation, so competing workers share whichever work is necessary.
        self.claimed(&memo, || {
            // Another worker may have completed while we waited for its claim.
            if ready() {
                {
                    let mut statistics = self.operation.stats();
                    statistics.hits += 1;
                    statistics.resident_hits += u64::from(want_value);
                }
                self.operation.diagnostic_reuse(
                    &memo.address,
                    &None,
                    if want_value {
                        ReuseReason::ResidentValue
                    } else {
                        ReuseReason::ValidityProof
                    },
                );
                return Ok(());
            }
            let database = self.operation.database;
            // A released header does not erase the proof that its result is still valid.
            // Recover the output metadata before deciding whether dependencies need checking.
            if memo.lock().header.is_none() {
                let verified = database.backend.verified(&memo.id);
                if verified == 0 || verified < database.changed_at[Durability::High as usize] {
                    // On restart, or after all durability classes changed, dependency
                    // validation is unavoidable. Read the complete record once instead
                    // of loading a header and immediately fetching that same row again.
                    self.load_record(&memo)?;
                } else {
                    let (header, read_disk) = database.backend.header(&memo.id)?;
                    self.operation.stats().header_reads += u64::from(read_disk);
                    let mut state = memo.lock();
                    if header.is_none() {
                        state.record_loaded = true;
                    }
                    state.header = header;
                }
                let mut state = memo.lock();
                if state.header.is_some() {
                    state.verified = verified;
                }
            }
            // Same-revision calls need only the header, even after the dependency record
            // was released. Durability can also prove that a later revision is unaffected.
            // Neither shortcut applies on restart, when verified is still zero.
            let mut reuse = {
                let state = memo.lock();
                state.header.as_ref().and_then(|header| {
                    if state.verified == database.revision {
                        Some(ReuseReason::ValidityProof)
                    } else if state.verified != 0
                        && database.changed_at[header.durability as usize] <= state.verified
                    {
                        Some(ReuseReason::Durability)
                    } else {
                        None
                    }
                })
            };
            if reuse.is_some() {
                memo.lock().verified = database.revision;
            } else if let Some(previous) = self.load_record(&memo)? {
                self.operation.stats().validated += 1;
                // Replayed child calls still need this producer identity for cycle detection.
                // They are checking the old dependency list, not collecting a replacement list.
                let frame = Frame::new(Some(memo.address.clone()));
                let observations = Db {
                    operation: self.operation,
                    frame: &frame,
                }
                .validate(&previous)?;
                if let Some((durability, lifetime)) = observations {
                    reuse = Some(ReuseReason::DependenciesValid);
                    let mut state = memo.lock();
                    // A child can keep the same answer after switching to different inputs.
                    // Inherit its new reuse restrictions even though our value remains valid.
                    if previous.header.durability != durability
                        || previous.header.lifetime != lifetime
                    {
                        let mut record = previous.as_ref().clone();
                        let header = Arc::make_mut(&mut record.header);
                        header.durability = durability;
                        header.lifetime = lifetime;
                        state.install_record(Arc::new(record));
                        state.dirty = true;
                    }
                    state.verified = database.revision;
                }
            }
            if let Some(reason) = reuse {
                // A valid query need not have a decoded answer. Stop here for validation-only
                // callers; ordinary calls must load a payload or reconstruct a transient value.
                if !want_value {
                    self.operation.stats().hits += 1;
                    self.operation
                        .diagnostic_reuse(&memo.address, &None, reason);
                    return Ok(());
                }
                if let Some(codec) = Q::storage().codec {
                    let current = memo.lock().header.clone().expect("verified header");
                    if let Some(value) = self.load(&memo.address, &current, &None, codec, false)? {
                        Q::validate(value.typed::<Q::Value>()?.as_ref())?;
                        memo.lock().values.insert(None, value);
                        self.operation.stats().hits += 1;
                        self.operation
                            .diagnostic_reuse(&memo.address, &None, reason);
                        return Ok(());
                    }
                }
            }
            // Execution may only be reconstructing a released transient value. Pass along
            // the validity proof so compute can keep its version without a value comparison.
            let previous = self.load_record(&memo)?;
            self.operation.diagnostic_execute(
                &memo.address,
                if reuse.is_some() {
                    "value_unavailable"
                } else if previous.is_some() {
                    "dependency_invalid"
                } else {
                    "no_record"
                },
            );
            self.compute::<Q>(&memo, key, previous.as_deref(), reuse.is_some())
        })?;
        Ok(memo)
    }

    /// Make the query's full dependency record available for validation or replacement. Reuse the
    /// operation's copy if present; otherwise fetch the last checkpoint's record. Checkpointing
    /// reads the durable baseline separately when it needs to replace outputs. The caller holds
    /// the query claim. An absent stored record is remembered just like a hit.
    fn load_record(&self, memo: &Memo) -> Result<Option<Arc<Record>>> {
        if !memo.lock().record_loaded {
            let (old, read_disk) = self.operation.database.backend.record(&memo.id)?;
            if read_disk {
                self.operation.stats().metadata_reads += 1;
            }
            let mut state = memo.lock();
            match old {
                Some(record) => state.install_record(record),
                None => {
                    state.header = None;
                    state.record = None;
                    state.verified = 0;
                    state.record_loaded = true;
                }
            }
        }
        Ok(memo.lock().record.clone())
    }

    /// Decide whether the reads saved by the previous execution still justify its answer.
    ///
    /// If `word_count(file)` read `words(file)` at W1, first establish that `words` is valid for
    /// the new inputs, then compare its version with W1. Adding whitespace may rerun `words`
    /// but leave its answer equal and its version W1. Then `word_count` need not run. If `words`'
    /// own inputs match directly, checking this dependency does not even need the words value.
    ///
    /// Stop at the first changed read and return None so the caller can execute the query again.
    /// If every read matches, return the dependencies' combined reuse policy. Their durability
    /// or persistence can change even when their answers remain equal.
    fn validate(&self, record: &Record) -> Result<Option<(Durability, Lifetime)>> {
        let _span = tracing::trace_span!("ajika.validate", query = record.address.family).entered();
        let mut durability = Durability::High;
        let mut lifetime = Lifetime::Persistent;
        // Do not inspect later reads after a changed guard. They may belong to an old branch
        // that the new execution will never enter, including an output that no longer exists.
        for dependency in &record.dependencies {
            self.check_cancelled()?;
            let (child_durability, child_lifetime) = match dependency {
                Dependency::Input(address, observed) => {
                    let Some(input) = self.operation.database.registry.stamp(self, address)? else {
                        self.operation.diagnostic_invalidated(
                            &record.address,
                            dependency,
                            "registration_unavailable",
                        );
                        return Ok(None);
                    };
                    if input.stamp != *observed {
                        self.operation.diagnostic_invalidated(
                            &record.address,
                            dependency,
                            "input_stamp_changed",
                        );
                        return Ok(None);
                    }
                    (input.durability, input.lifetime)
                }
                Dependency::Value(address, slot, observed) => {
                    let Some(memo) = self
                        .operation
                        .database
                        .registry
                        .replay(self, address, false)?
                    else {
                        self.operation.diagnostic_invalidated(
                            &record.address,
                            dependency,
                            "registration_unavailable",
                        );
                        return Ok(None);
                    };
                    let (version, durability, lifetime) = {
                        let state = memo.lock();
                        let child = state.header.as_ref().expect("verified dependency");
                        (
                            child.values.get(slot).map(|value| value.version),
                            child.durability,
                            child.lifetime,
                        )
                    };
                    // An omitted output invalidates the old path; it is not an error
                    // that prevents the parent from choosing a different new branch.
                    let Some(version) = version else {
                        self.operation.diagnostic_invalidated(
                            &record.address,
                            dependency,
                            "output_removed",
                        );
                        return Ok(None);
                    };
                    if version != *observed {
                        self.operation.diagnostic_invalidated(
                            &record.address,
                            dependency,
                            "dependency_version_changed",
                        );
                        return Ok(None);
                    }
                    (durability, lifetime)
                }
            };
            durability = durability.min(child_durability);
            lifetime = lifetime.min(child_lifetime);
        }
        Ok(Some((durability, lifetime)))
    }

    /// Run the query function and turn that execution into a result other queries can use.
    ///
    /// The function records fresh reads and outputs in its own frame. We compare each answer
    /// with its previous value, keeping the old version when equal, then install the new reads,
    /// versions, and values together in the memo. This is what lets equal recomputation stop
    /// invalidation from reaching callers while still updating the query's own dependencies.
    ///
    /// The caller holds the query claim until this finishes. `valid` means the old dependencies
    /// already proved the answer unchanged and execution is only reconstructing a missing value.
    /// Saving new work to the backend is deferred until the whole operation checkpoints.
    fn compute<Q: Query<Services = S>>(
        &self,
        memo: &Memo,
        key: Q::Key,
        previous: Option<&Record>,
        valid: bool,
    ) -> Result<()> {
        let _span = tracing::debug_span!("ajika.query", query = Q::NAME).entered();
        // Start a fresh read list. Equal results can take a different branch, so old dependencies
        // must be replaced even if all output versions are eventually kept.
        let frame = Frame::new(Some(memo.address.clone()));
        let db = Db {
            operation: self.operation,
            frame: &frame,
        };
        let primary = Q::execute(&db, key)?;
        self.check_cancelled()?;
        Q::validate(&primary)?;
        let reads = frame.finish(&mut self.operation.stats());
        let mut values = reads.outputs;
        values.insert(
            None,
            Value {
                data: primary,
                codec: Q::storage().codec,
            },
        );
        let mut records = BTreeMap::new();
        let mut changed = false;

        // Compare slots independently. For example, changing a function body may leave an
        // emitted declaration table equal while changing a separate body table.
        for (slot, value) in &values {
            let old_record = previous.and_then(|record| record.header.values.get(slot));
            // A missing transient value may have forced execution even though its old reads
            // were valid. Determinism already proves equality in that case; no codec is needed.
            let mut outcome = if old_record.is_none() {
                OutputOutcome::Added
            } else if valid {
                OutputOutcome::EqualByProof
            } else {
                OutputOutcome::ComparisonUnavailable
            };
            if !outcome.preserves_version()
                && let (Some(previous), Some(_), Some(codec)) = (previous, old_record, value.codec)
            {
                let resident = memo.lock().values.get(slot).cloned();
                let old_value = match resident {
                    Some(value) => Some(value),
                    None => self.load(&previous.address, &previous.header, slot, codec, true)?,
                };
                if let Some(old) = old_value {
                    let _span = tracing::trace_span!("ajika.compare", query = Q::NAME).entered();
                    outcome = if (codec.equal)(&old.data, &value.data)? {
                        OutputOutcome::EqualByComparison
                    } else {
                        OutputOutcome::Unequal
                    };
                }
            }
            let record = if outcome.preserves_version() {
                self.operation.stats().unchanged += 1;
                old_record.expect("equal result has an old version").clone()
            } else {
                changed = true;
                ValueRecord::fresh()?
            };
            if value.codec.is_some() && record.length.is_none() {
                // Equality can preserve a version whose payload was never durable
                // (for example after request-only inputs). Persist these fresh bytes.
                changed = true;
            }
            records.insert(slot.clone(), record);
            self.operation
                .diagnostic_output(&memo.address, slot, outcome);
        }
        // Removed slots have no new value to visit in the comparison loop. Only diagnostics
        // need this extra walk; the changed output metadata already makes the record dirty.
        if tracing::enabled!(target: diagnostics::TARGET, tracing::Level::TRACE)
            && let Some(previous) = previous
        {
            for slot in previous
                .header
                .values
                .keys()
                .filter(|slot| !records.contains_key(*slot))
            {
                self.operation
                    .diagnostic_output(&memo.address, slot, OutputOutcome::Removed);
            }
        }

        self.check_cancelled()?;
        // Materializing an evicted syntax value can leave every observation and
        // version unchanged. Keep that operation read-only instead of checkpointing
        // identical metadata merely because a provider had to run again.
        let dirty = changed
            || previous.is_none_or(|old| {
                old.dependencies != reads.dependencies
                    || old.header.values != records
                    || old.header.durability != reads.durability
                    || old.header.lifetime != reads.lifetime
            });
        let record = Arc::new(Record {
            address: memo.address.clone(),
            dependencies: reads.dependencies,
            header: Arc::new(RecordHeader {
                values: records,
                durability: reads.durability,
                lifetime: reads.lifetime,
            }),
            last_used: self.operation.now,
            bytes: previous.map_or(0, |old| old.bytes),
        });
        // Publish values, their versions, and the new reads together. Before this point an error
        // leaves no partial output set for a waiting query to observe.
        let mut state = memo.lock();
        state.install_record(record);
        state.values = values;
        state.dirty |= dirty;
        state.verified = self.operation.database.revision;
        self.operation.stats().executed += 1;
        Ok(())
    }

    /// Fetch one stored payload for reuse or for comparison with a newly computed answer.
    /// The header supplies its encoded length; the address and slot locate it. Returning None
    /// means the backend has no retained payload described by this header. The caller decides
    /// whether to recompute. This step accounts for decoding cost but does not decide validity
    /// or record a dependency.
    fn load(
        &self,
        address: &Address,
        header: &RecordHeader,
        slot: &Slot,
        codec: Codec,
        comparison: bool,
    ) -> Result<Option<Value>> {
        let _span = tracing::trace_span!("ajika.load", query = address.family).entered();
        let value = self
            .operation
            .database
            .backend
            .value(address, header, slot, codec, &|| self.check_cancelled())?;
        if value.as_ref().is_some_and(|(_, decoded)| *decoded) {
            let mut statistics = self.operation.stats();
            statistics.decoded += 1;
            statistics.compared += u64::from(comparison);
            statistics.bytes_read += header
                .values
                .get(slot)
                .and_then(|value| value.length)
                .unwrap_or(0);
        }
        Ok(value.map(|(value, _)| value))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{Database, Db, Inputs, Result, Retention};

    #[crate::query(crate = crate, transient, returns(ref))]
    fn label(_db: &Db<'_>, number: u32) -> Result<String> {
        Ok(number.to_string())
    }

    #[test]
    fn borrowed_query_answers_share_the_owned_answer() -> Result<()> {
        let mut database = Database::memory(
            Inputs::default(),
            (),
            Retention {
                max_entries: 1,
                max_bytes: 1024,
                max_idle: Duration::from_secs(60),
            },
        );
        database.run(|db| {
            let first = label(db, 7)?;
            assert!(std::ptr::eq(first, db.query::<label>(7)?.as_ref()));
            assert_eq!(db.query_ref::<label>(8)?, "8");
            assert_eq!(first, "7");
            assert!(std::ptr::eq(first, db.query_ref::<label>(7)?));
            Ok(())
        })
    }
}
