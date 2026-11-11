//! Sharing unfinished query work and stopping workers within an operation.
//!
//! Parallel callers can ask for the same result before it is ready. [`Coordination`] lets one
//! caller work on it and others wait, using a [`Claim`] to reserve that query call. It also detects
//! calls that would wait on themselves and shares failures across workers. The host-facing
//! [`Cancellation`] token uses the same mechanism to request that the whole operation stop.

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    fmt,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use super::{diagnostics::Hex, record::QueryId};
use crate::{Cancelled, Error, Result};

/// A handle the host can use to request that one operation stop, usually from another thread.
/// Clones signal the same request. Get it from the database before [`run`](super::Database::run);
/// cancellation requested before work starts is remembered too. Later operations receive a fresh
/// token.
///
/// Cancellation is cooperative: Ajika checks it while serving queries and doing storage work. Long
/// application computations must check through [`Db::check_cancelled`](super::Db::check_cancelled)
/// themselves.
#[derive(Clone, Default)]
pub struct Cancellation {
    pub(crate) inner: Arc<Coordination>,
}

impl Cancellation {
    /// Request cancellation and wake workers waiting for another query. This does not wait
    /// for them to finish or forcibly interrupt a running query function.
    pub fn cancel(&self) {
        // Pair the notification with the waiter's mutex. Otherwise cancellation could happen
        // between its flag check and going to sleep, leaving it waiting for a missed wakeup.
        let _state = self.inner.state.lock().expect("claim state");
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.ready.notify_all();
    }

    /// Whether the host explicitly requested cancellation. Query failures can also stop work; query
    /// code uses [`Db::check_cancelled`](super::Db::check_cancelled) to observe either reason.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }
}

/// One query/key pair participating in a [`QueryCycle`]. Keys stay opaque: query authors do not
/// need to implement Debug or expose source text to make a cycle identifiable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryCall {
    /// Registered query family name.
    pub query: String,
    /// Full address hash, matching the diagnostics target's `call_id` field.
    pub id: [u8; 32],
}

impl fmt::Display for QueryCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.query, Hex(&self.id[..4]))
    }
}

/// The active calls that would wait on each other, within one thread or across workers.
///
/// [`calls`](Self::calls) follows parent-to-child edges and repeats the first call at the end.
/// It is one cycle witness, not every active call or the complete saved dependency graph.
/// Hosts can inspect [`crate::ErrorKind::Cycle`] even when tracing is off.
/// Diagnostic context and worker sharing preserve that classification.
#[derive(Clone, Debug)]
pub struct QueryCycle {
    pub calls: Vec<QueryCall>,
}

impl fmt::Display for QueryCycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("query cycle:")?;
        for (index, call) in self.calls.iter().enumerate() {
            if index == 0 {
                write!(f, "\n  {call}")?;
            } else {
                write!(f, "\n    → {call}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for QueryCycle {}

/// Which query calls have unfinished work and how they are nested across all workers.
/// This temporary call graph lets us detect a cycle even when its calls are on different
/// thread stacks. It is discarded with the operation; saved query dependencies live elsewhere.
#[derive(Default)]
struct State {
    /// Borrow declaration names for active claims; ordinary validation and loading allocate no
    /// name copies. Only an actual cycle needs owned names for its error report.
    running: HashMap<QueryId, &'static str>,
    // These are active nested calls, not the saved dependency graph. Include calls doing work
    // as well as calls waiting for a claim: both can close a cycle. Parallel branches may make
    // the same parent -> child call, so one branch finishing must not remove another's edge.
    // Group children by parent so traversal only examines each node's outgoing calls.
    calls: HashMap<QueryId, HashMap<QueryId, usize>>,
    failure: Option<Error>,
}

impl State {
    /// Find an active path from child back to parent before adding parent -> child. Keep the
    /// successful route so a cycle error can identify the actual calls, not just the last one.
    fn path(
        &self,
        from: QueryId,
        to: QueryId,
        seen: &mut HashSet<QueryId>,
        path: &mut Vec<QueryId>,
    ) -> bool {
        // Only running queries own outgoing calls. A request for a free key cannot
        // close a cycle, so it needs neither a graph scan nor a visited-set allocation.
        if from == to {
            path.push(from);
            return true;
        }
        if !self.running.contains_key(&from) || !seen.insert(from) {
            return false;
        }
        path.push(from);
        if let Some(children) = self.calls.get(&from) {
            for child in children.keys() {
                if self.path(*child, to, seen, path) {
                    return true;
                }
            }
        }
        path.pop();
        false
    }
}

/// Coordinates who may work on each query call in one operation.
///
/// If two workers request `words("lib.rs")`, one claims the call while the other waits.
/// Once the owner releases its claim, the waiter checks the memo and can use the finished answer.
/// Different keys can progress concurrently. A claim covers validity checks and decoding too,
/// so callers share that work even when no query function needs to run.
///
/// The same coordinator records active parent-to-child calls, failures, and cancellation.
/// A cycle or failure must wake waiters as well as stop running workers. The mutex protects
/// this bookkeeping only; query functions and value decoding execute after it has been released.
#[derive(Default)]
pub(crate) struct Coordination {
    state: Mutex<State>,
    ready: Condvar,
    cancelled: AtomicBool,
    failed: AtomicBool,
}

impl Coordination {
    /// Report why the worker must stop. A recorded failure takes precedence over cancellation.
    pub(crate) fn check(&self) -> Result<()> {
        if self.failed.load(Ordering::Acquire) {
            let failure = self
                .state
                .lock()
                .expect("claim state")
                .failure
                .clone()
                .expect("failed operation has a cause");
            return Err(failure);
        }
        if self.cancelled.load(Ordering::Acquire) {
            return Err(Cancelled::at("query operation").into());
        }
        Ok(())
    }

    /// Record why work stopped and wake every waiter. Keep the first failure even if a caller
    /// catches it. If cancellation arrived first, a later failure replaces it so the host can
    /// still report the cause.
    pub(crate) fn fail(&self, failure: Error) {
        let mut state = self.state.lock().expect("claim state");
        if state.failure.is_none()
            || state.failure.as_ref().is_some_and(Error::is_cancelled) && !failure.is_cancelled()
        {
            state.failure = Some(failure);
        }
        self.failed.store(true, Ordering::Release);
        self.ready.notify_all();
    }

    /// Reserve work on a query call, waiting if another worker already owns it.
    ///
    /// `parent` identifies the executing query asking for this child; host calls have no parent.
    /// Record that relationship before waiting so a chain such as `a -> b -> a` fails instead of
    /// deadlocking. Cancellation and prior failures also end the wait with an error.
    ///
    /// The returned guard owns the reservation, not the mutex. Its caller must recheck the memo
    /// before doing work, because the previous owner may have completed it during the wait.
    pub(crate) fn claim(
        &self,
        parent: Option<QueryId>,
        key: QueryId,
        name: &'static str,
    ) -> Result<Claim<'_>> {
        // Declare the guard before the lock so early returns release the lock first.
        let mut claim = Claim {
            coordination: self,
            key,
            edge: None,
            acquired: false,
            complete: false,
            waited: false,
        };
        let mut state = self.state.lock().expect("claim state");
        if let Some(parent) = parent {
            // Adding parent -> key closes a cycle exactly when key already reaches parent.
            // Check before waiting: otherwise the participating workers could all go to sleep.
            let mut path = Vec::new();
            if state.path(key, parent, &mut HashSet::new(), &mut path) {
                path.push(key);
                let cycle = QueryCycle {
                    calls: path
                        .into_iter()
                        .map(|id| QueryCall {
                            query: state
                                .running
                                .get(&id)
                                .copied()
                                .expect("cycle has active claims")
                                .to_owned(),
                            id: id.0,
                        })
                        .collect(),
                };
                // Copy the witness while its claims exist, but leave error construction and
                // formatting outside the coordinator lock. The normal failure path wakes waiters.
                drop(state);
                return Err(cycle.into());
            }
            *state
                .calls
                .entry(parent)
                .or_default()
                .entry(key)
                .or_default() += 1;
            claim.edge = Some((parent, key));
        }
        loop {
            if let Some(error) = &state.failure {
                let error = error.clone();
                drop(state);
                return Err(error);
            }
            if self.cancelled.load(Ordering::Acquire) {
                return Err(Cancelled::at("query operation").into());
            }
            if let Entry::Vacant(entry) = state.running.entry(key) {
                entry.insert(name);
                claim.acquired = true;
                return Ok(claim);
            }
            claim.waited = true;
            state = self.ready.wait(state).expect("claim state");
        }
    }

    /// Check the lifecycle rule that all query work has finished before checkpoint or cleanup.
    /// This does not join threads; scoped callers must already have joined and released their claims.
    pub(crate) fn assert_drained(&self) {
        let state = self.state.lock().expect("claim state");
        assert!(state.running.is_empty(), "operation has running queries");
        assert!(state.calls.is_empty(), "operation has active query calls");
    }
}

/// A worker's reservation to validate, compute, or decode one query call.
///
/// Keeping this guard alive makes other workers wait for that key while allowing unrelated query
/// calls to continue. Releasing it removes the reservation and its parent-to-child call edge, then
/// wakes waiters. The evaluator calls [`complete`](Claim::complete) after handling the work's
/// [`Result`]. Dropping an unfinished claim, for example during a panic, also fails the operation
/// so another worker does not try to continue from an abandoned computation.
pub(crate) struct Claim<'a> {
    coordination: &'a Coordination,
    key: QueryId,
    // Registered before waiting; it needs removal even if this worker never acquires the key.
    edge: Option<(QueryId, QueryId)>,
    acquired: bool,
    complete: bool,
    pub(crate) waited: bool,
}

impl Claim<'_> {
    /// Release a reservation after its [`Result`] has been handled. This also covers an ordinary
    /// error that was already recorded on the operation; "complete" does not imply success.
    pub(crate) fn complete(mut self) {
        self.complete = true;
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut state = self.coordination.state.lock().expect("claim state");
        if self.acquired {
            state.running.remove(&self.key);
            if !self.complete {
                // An unwinding provider has no ordinary Result to report. Stop the operation
                // before waking waiters so they do not retry work from the abandoned query.
                if state.failure.as_ref().is_none_or(Error::is_cancelled) {
                    state.failure = Some(Error::invariant("query execution abandoned"));
                }
                self.coordination.failed.store(true, Ordering::Release);
            }
        }
        if let Some((parent, child)) = self.edge {
            let children = state
                .calls
                .get_mut(&parent)
                .expect("registered parent calls");
            let count = children.get_mut(&child).expect("registered query call");
            *count -= 1;
            if *count == 0 {
                children.remove(&child);
            }
            if children.is_empty() {
                state.calls.remove(&parent);
            }
        }
        self.coordination.ready.notify_all();
    }
}
