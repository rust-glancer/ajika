//! Query execution and reuse over the application's captured inputs.
//!
//! The host uses [`Database`] to supply inputs and start an operation, such as indexing or
//! answering a request. Query functions receive a [`Db`] through which they read inputs and call
//! other queries. This module connects those calls to remembered answers, dependency tracking,
//! parallel work, and storage. Query implementations do not need to manage that lifecycle
//! themselves.

use std::{
    collections::HashMap,
    fmt, panic,
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{Error, Result, Retention, Statistics};

mod backend;
mod cache;
mod coordination;
mod diagnostics;
mod evaluate;
mod frame;
mod output;
mod parallel;
mod record;
pub(crate) mod registry;
pub(crate) mod value;

use self::{
    backend::Backend,
    cache::{Memo, Memos},
    frame::Frame,
    record::Id,
    registry::Registry,
};
pub use self::{
    coordination::{Cancellation, QueryCall, QueryCycle},
    output::{Output, OutputSlot},
};

/// The application's long-lived query database.
///
/// It owns the captured inputs and remembers previous query calls so later work can reuse them. The
/// host supplies a new input view with [`replace_inputs`](Database::replace_inputs), then calls
/// [`run`](Database::run) to ask queries against it. A run may contain one query or an entire
/// indexing job; inside it, query functions receive [`Db`] rather than mutable access to this
/// owner.
///
/// An input revision and an operation are different boundaries. Indexing and several later
/// requests can all use the same revision. Starting another operation releases and reloads
/// working values as needed, but does not itself make previously checked answers uncertain.
/// Only changed inputs or input policies advance the revision. Operations and replacements are exclusive
/// on this owner; parallel query work happens inside an operation.
pub struct Database<S: Send + Sync + 'static = ()> {
    inputs: crate::Inputs,
    services: S,
    backend: Backend,
    registry: Registry<S>,
    // Process-local revisions are reuse shortcuts, not persistent value versions.
    revision: u64,
    // Last revision that may have changed each durability class. A query depending only on
    // High inputs can skip a dependency walk after a change limited to Low inputs.
    changed_at: [u64; 3],
    cancellation: Cancellation,
    parallelism: usize,
    retention: Retention,
    session_retention: Retention,
    last_collection: Instant,
    statistics: Statistics,
}

impl<S: Send + Sync + 'static> Database<S> {
    /// Keep reusable results as typed Rust values without opening a storage file.
    /// Answers of persistent-policy queries can survive between operations in this database instance;
    /// transient query values and request-only results are still released at operation end.
    pub fn memory(inputs: crate::Inputs, services: S, retention: Retention) -> Self {
        Self::new(inputs, services, Backend::memory(), retention)
    }

    /// Open or create a database whose saved results can survive process restarts.
    /// Old records are checked against `inputs` when requested, not assumed valid merely because
    /// they exist. A different application epoch discards them when opening the directory.
    ///
    /// Change the epoch when query semantics or stored types change. Ajika detects changes in
    /// tracked data, but cannot infer that editing a query's Rust implementation changes its answer.
    pub fn open(
        path: impl AsRef<Path>,
        epoch: &str,
        inputs: crate::Inputs,
        services: S,
        retention: Retention,
    ) -> Result<Self> {
        Ok(Self::new(
            inputs,
            services,
            Backend::open(path.as_ref(), epoch)?,
            retention,
        ))
    }

    fn new(inputs: crate::Inputs, services: S, backend: Backend, retention: Retention) -> Self {
        Self {
            inputs,
            services,
            backend,
            registry: Registry::linked(),
            revision: 1,
            changed_at: [1; 3],
            cancellation: Cancellation::default(),
            parallelism: thread::available_parallelism().map_or(1, usize::from),
            retention,
            session_retention: Retention {
                max_entries: 16_384,
                max_bytes: 64 * 1024 * 1024,
                max_idle: Duration::from_secs(300),
            },
            last_collection: Instant::now(),
            statistics: Statistics::default(),
        }
    }

    /// Get a token the host can send to another thread to cancel the next or active operation.
    /// Obtain a fresh token for each run; a completed run or input replacement retires the old one.
    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    /// Counters from the latest operation, with any subsequent collection included.
    pub fn statistics(&self) -> Statistics {
        self.statistics
    }

    /// Choose the space/CPU policy for future disk writes. Existing values keep their own codec.
    /// Memory storage does not encode values and ignores this setting.
    pub fn set_compression(&mut self, compression: crate::Compression) {
        self.backend.set_compression(compression);
    }

    /// Inspect physical storage without decoding query values. This scans storage metadata,
    /// so use it for diagnostics. Returns `None` for memory storage.
    pub fn storage_statistics(&self) -> Result<Option<crate::StorageStatistics>> {
        self.backend.storage_statistics()
    }

    /// Get the shared interner for source capture or query providers.
    /// Open the database before constructing inputs that contain its IDs, then keep using this
    /// interner across input revisions. Reopening loads the saved assignments before returning.
    pub fn interner(&self) -> crate::Interner {
        self.backend.interner.clone()
    }

    pub fn inputs(&self) -> &crate::Inputs {
        &self.inputs
    }

    pub fn services(&self) -> &S {
        &self.services
    }

    /// Replace nonsemantic host services between operations. Values affecting query answers
    /// belong in typed inputs instead; changing services does not invalidate any query.
    pub fn set_services(&mut self, services: S) {
        self.services = services;
    }

    /// Publish a coherent batch of typed input changes. Errors discard the private batch.
    /// Equal values keep their validity proofs; changed retention policies are observed too.
    pub fn update_inputs(
        &mut self,
        edit: impl FnOnce(&mut crate::InputUpdate) -> Result<()>,
    ) -> Result<()> {
        self.replace_inputs(self.inputs.edit(edit)?);
        Ok(())
    }

    /// Bound work retained for session inputs. The default is 64 MiB of estimated encoded data,
    /// 16,384 producers, and five minutes idle. Values remain typed; measuring their encoded size
    /// neither writes nor allocates an encoded copy. Actual Rust allocation sizes can differ.
    pub fn set_session_retention(&mut self, retention: Retention) {
        self.session_retention = retention;
    }

    /// Release editor-derived work while preserving the durable baseline. For example, a host
    /// can trim it when the last edited buffer is discarded. Later demands validate or recompute.
    pub fn clear_session(&mut self) {
        self.backend.clear_session();
        (
            self.statistics.session_queries,
            self.statistics.session_bytes,
        ) = self.backend.session_usage();
    }

    /// Limit the threads used by [`Db::parallel_map`], including its calling thread.
    /// Nested batches share this budget; setting it to one makes those batches run sequentially.
    pub fn set_parallelism(&mut self, workers: usize) {
        assert!(workers > 0, "parallelism must be positive");
        self.parallelism = workers;
    }

    /// Install a captured input snapshot. The database compares input fingerprints and policies
    /// to identify affected durability classes. Restoring an older snapshot is a new revision,
    /// never a rewind of cached query state. Operations and replacements are exclusive.
    pub fn replace_inputs(&mut self, inputs: crate::Inputs) {
        let changed = inputs.changed_since(&self.inputs);
        self.inputs = inputs;
        if let Some(changed) = changed {
            self.revision = self
                .revision
                .checked_add(1)
                .expect("revision counter exhausted");
            self.changed_at[..=changed as usize].fill(self.revision);
        }
        self.cancellation = Cancellation::default();
    }

    /// Run a unit of host work whose queries can share loaded and computed values.
    ///
    /// The callback receives a [`Db`] for the current captured inputs. Query answers become usable
    /// by other queries as soon as they are computed. Once the callback and its scoped workers
    /// finish successfully, a checkpoint saves the operation's changes together. Ajika releases
    /// the operation's working state when the run ends.
    ///
    /// Failure before checkpointing discards pending work, including successful child queries.
    /// The host should reject inconsistent captured inputs before returning from the callback.
    /// A cancellation or maintenance error after commit can make this return an error even
    /// though the cache was saved.
    ///
    /// The callback uses the same [`Result`] as query providers. A recorded query failure takes
    /// precedence over cancellation, including cancellation returned directly by the callback.
    /// Other callback errors retain their context and application-specific cause. Use
    /// [`Error::is_cancelled`] when deciding whether to publish the answer.
    ///
    /// Returned [`Arc`]s keep their answers alive after the run, including across input changes.
    pub fn run<R>(&mut self, work: impl FnOnce(&Db<'_, S>) -> Result<R>) -> Result<R> {
        let _span = tracing::debug_span!("ajika.operation", revision = self.revision).entered();
        // Reset cancellation after a panic too, so a host that catches it can start another
        // operation. Working memos and query claims clean up through their own scopes.
        let outcome = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let operation = Operation::new(self);
            let frame = Frame::new(None);
            let db = Db {
                operation: &operation,
                frame: &frame,
            };
            let result = (|| {
                db.check_cancelled()?;
                // Providers can publish completed values to each other in memory. Nothing is
                // written to disk until the host callback and all its scoped work have finished.
                let callback_result = work(&db);
                // Cancellation means an answer is no longer needed, but must not conceal a
                // failure that a provider already recorded. Preserve real callback errors so
                // application-specific causes and any added context survive this boundary.
                if callback_result.as_ref().is_err_and(Error::is_cancelled)
                    && let Err(recorded) = self.cancellation.inner.check()
                    && !recorded.is_cancelled()
                {
                    return Err(recorded);
                }
                let value = callback_result?;
                db.check_cancelled()?;
                self.cancellation.inner.assert_drained();
                let memos = operation
                    .touched
                    .lock()
                    .expect("operation memos")
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                // The checkpoint writes the completed producers together, including new
                // dependency lists for results whose payloads compared equal.
                self.backend.checkpoint(
                    &memos,
                    operation.now,
                    self.parallelism,
                    &mut operation.stats(),
                    &|| db.check_cancelled(),
                )?;
                // A completed commit may survive this cancellation. It remains cache
                // data with its actual input observations, not an accepted project.
                db.check_cancelled()?;
                // A proof describes the record that was actually accepted. Do not publish
                // proofs for pending replacements when the callback or checkpoint fails.
                self.backend.remember_verified(&memos);
                Ok(value)
            })();
            frame.finish(&mut operation.stats());
            let statistics = *operation.stats();
            // Leaving this scope releases the working set on success and error alike.
            (result, statistics)
        }));
        self.cancellation = Cancellation::default();
        match outcome {
            Ok((result, statistics)) => {
                self.statistics = statistics;
                if result.is_ok()
                    && (self.last_collection.elapsed() >= Duration::from_secs(60)
                        || self.backend.over_budget(self.retention))
                {
                    self.collect()?;
                }
                (self.statistics.stored_queries, self.statistics.stored_bytes) =
                    self.backend.usage();
                let removed = self.backend.collect_session(self.session_retention);
                self.statistics.collected += removed as u64;
                // Relocation needs neither query execution nor the released working values.
                // A capacity refusal still permits this pass, so reclamation can make progress
                // without poisoning otherwise readable cached results.
                if result.is_ok() || result.as_ref().is_err_and(Error::is_capacity) {
                    self.backend.maintain()?;
                }
                (
                    self.statistics.session_queries,
                    self.statistics.session_bytes,
                ) = self.backend.session_usage();
                tracing::debug!(
                    succeeded = result.is_ok(),
                    statistics = ?self.statistics,
                    "query operation finished"
                );
                result
            }
            Err(panic) => panic::resume_unwind(panic),
        }
    }

    /// Apply retention limits to stored and session results. A later request can recompute an
    /// evicted query and its outputs, including when needed by a retained query. Returns the
    /// number of query records removed.
    ///
    /// Successful runs already collect periodically or when over budget. The host can request
    /// this pass at another idle boundary too. Interned IDs remain valid.
    pub fn collect(&mut self) -> Result<usize> {
        let mut removed = self.backend.collect(self.retention)?;
        removed += self.backend.collect_session(self.session_retention);
        self.backend.interner.reallocate();
        self.last_collection = Instant::now();
        self.statistics.collected += removed as u64;
        self.backend.maintain()?;
        (self.statistics.stored_queries, self.statistics.stored_bytes) = self.backend.usage();
        (
            self.statistics.session_queries,
            self.statistics.session_bytes,
        ) = self.backend.session_usage();
        Ok(removed)
    }

    /// Discard all remembered query results, both in storage and in the running database.
    /// The captured inputs stay installed; the next operation computes its answers from them
    /// without reusing old query work. No operation may be active during this reset.
    pub fn invalidate(&mut self) -> Result<()> {
        self.backend.invalidate()?;
        Ok(())
    }

    /// Finish physical reclamation and shrink the index at an idle boundary. Unlike the small
    /// automatic passes between operations, this call can copy all remaining live payloads.
    /// It changes no query results or validity, and is a no-op for memory storage.
    pub fn compact(&mut self) -> Result<bool> {
        self.backend.compact()
    }
}

/// The working scope shared by every query in one [`Database::run`] callback.
///
/// A host operation may ask many queries, possibly in parallel. Their loaded and computed values
/// should remain available to each other until that work finishes. This object gathers the memos
/// they use, their statistics, and their worker budget. At the end, the gathered memos identify
/// what must be checkpointed and which working values must be released.
///
/// It borrows the database's captured inputs for the whole run. This scope bounds working memory;
/// it does not advance the input revision or make previous answers invalid.
struct Operation<'db, S: Send + Sync + 'static> {
    // Global to this process, so events from different databases cannot share an operation ID.
    id: u64,
    database: &'db Database<S>,
    memos: Memos,
    touched: Mutex<HashMap<Id, Arc<Memo>>>,
    statistics: Mutex<Statistics>,
    spare: AtomicUsize,
    now: u64,
}

impl<'db, S: Send + Sync + 'static> Operation<'db, S> {
    fn new(database: &'db Database<S>) -> Self {
        static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_OPERATION.fetch_add(1, Ordering::Relaxed),
            database,
            memos: Memos::default(),
            touched: Mutex::default(),
            statistics: Mutex::default(),
            spare: AtomicUsize::new(database.parallelism - 1),
            now: Backend::now(),
        }
    }

    fn stats(&self) -> MutexGuard<'_, Statistics> {
        self.statistics.lock().expect("operation statistics")
    }

    /// Enroll a used memo in checkpointing and cleanup, even when this call only reused it.
    /// This avoids scanning every memo at operation end and refreshes the stored result's age
    /// so retention does not discard frequently read answers merely because they never recompute.
    fn touch(&self, memo: &Arc<Memo>) {
        self.touched
            .lock()
            .expect("operation memos")
            .entry(memo.id)
            .or_insert_with(|| {
                // The operation has one timestamp. Further reads of this memo cannot make
                // its retention age more recent, so refresh backend bookkeeping only once.
                self.database.backend.touch(&memo.id, self.now);
                memo.clone()
            });
    }
}

impl<S: Send + Sync + 'static> Drop for Operation<'_, S> {
    fn drop(&mut self) {
        self.database.cancellation.inner.assert_drained();
    }
}

/// Access to inputs and query answers during an operation.
///
/// The host callback and its queries use the same captured inputs for the run. Ajika loads or
/// computes answers as needed, and reads made inside a query automatically become its dependencies.
pub struct Db<'operation, S: Send + Sync + 'static = ()> {
    operation: &'operation Operation<'operation, S>,
    frame: &'operation Frame,
}

impl<S: Send + Sync + 'static> Db<'_, S> {
    /// Access the shared interner without recording a query dependency.
    pub fn interner(&self) -> crate::Interner {
        self.operation.database.interner()
    }

    /// Reach host services carried alongside captured inputs, such as progress reporting or
    /// allocator hooks. This access records no dependency. Data that determines the answer, such as
    /// source text or compiler options, must instead go through a tracked [`input`](Db::input)
    /// read.
    pub fn services(&self) -> &S {
        &self.operation.database.services
    }

    /// Return an error if this operation must stop: cancellation was requested, another worker
    /// failed, or storage became unusable. Query entry points check this automatically; long
    /// provider loops should also call it between chunks because cancellation cannot interrupt
    /// arbitrary application code on its own.
    pub fn check_cancelled(&self) -> Result<()> {
        self.operation.database.backend.check()?;
        self.operation.database.cancellation.inner.check()
    }

    /// Share the reason work stopped before returning it to the caller. When a provider catches
    /// a child error, the coordinator still stops the operation and prevents a checkpoint.
    /// For example, return syntax diagnostics as part of the answer if partial analysis is useful.
    fn handle<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        work().map_err(|error| {
            let coordination = &self.operation.database.cancellation.inner;
            coordination.fail(error.clone());
            // A worker that only observes cancellation must not hide another worker's cause.
            if error.is_cancelled() {
                coordination.check().expect_err("operation has stopped")
            } else {
                error
            }
        })
    }

    /// Do work for one memo while other workers requesting that call wait.
    /// The claim reserves this query/key pair without holding a mutex across `work`; different
    /// queries can continue concurrently. The closure must first recheck whether its work is
    /// still needed, because a previous claim owner may have finished it while this caller waited.
    fn claimed<T>(&self, memo: &Memo, work: impl FnOnce() -> Result<T>) -> Result<T> {
        let coordination = &self.operation.database.cancellation.inner;
        let parent = self.frame.owner.as_ref().map(|owner| owner.id());
        let claim = coordination.claim(parent, memo.id, memo.query_name)?;
        if claim.waited {
            self.operation.stats().waits += 1;
        }
        // Record ordinary errors before releasing the claim, so waiters see the
        // failure rather than starting the same broken work again. Drop handles panic.
        let result = self.handle(work);
        claim.complete();
        result
    }
}

impl<S: Send + Sync + 'static> fmt::Debug for Db<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Db")
            .field("revision", &self.operation.database.revision)
            .finish_non_exhaustive()
    }
}
