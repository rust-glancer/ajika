//! Dependency recording while a query function is executing.
//!
//! Query authors call inputs and other queries through [`Db`](super::Db); they do not assemble
//! dependency lists themselves. The runtime reports those reads to a [`Frame`], which collects what
//! this execution used. On completion, its [`Reads`] supply the dependencies for the saved query
//! record and any separately emitted outputs. Parallel callbacks collect their own parts and merge
//! them.

use std::{
    any::{Any, TypeId},
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
};

use super::{
    record::{Address, Dependency, Slot},
    value::{Data, Value},
};
use crate::{Durability, Lifetime, Query, Result, Statistics};

/// The temporary log for one execution of a query function.
///
/// For example, while `word_count(file)` runs, its frame records that it read the result of
/// `words(file)` at version W1. The child has its own frame for reading source text. Keeping
/// these logs separate means `word_count` depends on the words, not directly on the source:
/// a source edit that leaves the words equal need not make `word_count` run again.
///
/// [`Db`](super::Db) carries the active frame so reads can be recorded as they happen. When the
/// function returns, [`finish`](Frame::finish) hands the log to the evaluator to build the new
/// result record. A failed execution drops its frame without publishing the partial log or its
/// pending outputs.
pub(crate) struct Frame {
    /// Query whose function is running. Parallel branches retain that same owner. A parallel
    /// batch started directly by the host has no owning query, so its branch owners are None.
    pub(crate) owner: Option<Address>,
    pub(crate) reads: Mutex<Reads>,
    acquired: Mutex<Acquired>,
}

impl Frame {
    pub(crate) fn new(owner: Option<Address>) -> Self {
        Self {
            owner,
            reads: Mutex::default(),
            acquired: Mutex::default(),
        }
    }

    /// Record a value that the running query just used, including the stamp or version it saw.
    /// Later validation will check that same observation before trusting this execution's answer.
    /// The input/query read paths call this on the caller's frame after obtaining the value.
    pub(crate) fn depend(
        &self,
        dependency: Dependency,
        durability: Durability,
        lifetime: Lifetime,
    ) {
        if self.owner.is_none() {
            return;
        }
        self.reads
            .lock()
            .expect("query reads")
            .depend(dependency, durability, lifetime);
    }

    /// Reuse a read made through this frame, without revisiting the producer's shared locks.
    /// The caller still checks cancellation. A miss must use normal evaluation and record its
    /// dependency before installing the value here.
    pub(crate) fn acquired<T: Send + Sync + 'static>(
        &self,
        producer: &Address,
        slot: &[u8],
    ) -> Result<Option<Arc<T>>> {
        let mut acquired = self.acquired.lock().expect("frame values");
        let Some(value) = acquired
            .outputs
            .get(producer)
            .and_then(|slots| slots.get(slot))
        else {
            return Ok(None);
        };
        let value = value
            .clone()
            .downcast()
            .map_err(|_| anyhow::anyhow!("query value type changed"))?;
        acquired.hits += 1;
        Ok(Some(value))
    }

    pub(crate) fn remember(&self, producer: &Address, slot: &[u8], value: Data) {
        self.acquired
            .lock()
            .expect("frame values")
            .outputs
            .entry(producer.clone())
            .or_default()
            .insert(slot.to_vec(), value);
    }

    /// Ordinary query calls use their typed key directly, avoiding serialization merely to
    /// find an answer this frame already observed. The table lives only until this execution ends.
    pub(crate) fn query<Q: Query>(&self, key: &Q::Key) -> Option<Arc<Q::Value>> {
        let mut acquired = self.acquired.lock().expect("frame values");
        let values = acquired
            .queries
            .get(&TypeId::of::<Q>())?
            .downcast_ref::<HashMap<Q::Key, Arc<Q::Value>>>()
            .expect("query table type");
        let value = values.get(key)?.clone();
        acquired.hits += 1;
        Some(value)
    }

    pub(crate) fn remember_query<Q: Query>(&self, key: Q::Key, value: Arc<Q::Value>) {
        let mut acquired = self.acquired.lock().expect("frame values");
        acquired
            .queries
            .entry(TypeId::of::<Q>())
            .or_insert_with(|| Box::new(HashMap::<Q::Key, Arc<Q::Value>>::new()))
            .downcast_mut::<HashMap<Q::Key, Arc<Q::Value>>>()
            .expect("query table type")
            .insert(key, value);
    }

    /// End recording and hand the collected reads and outputs to the caller. For a query,
    /// they contribute to its new result record; for a parallel branch, they are merged into the parent.
    /// This only transfers the log. Comparing results and saving them happen afterward.
    pub(crate) fn finish(self, statistics: &mut Statistics) -> Reads {
        // Count repeated reads once per execution, rather than taking a shared statistics
        // lock on every field lookup. Acquired values leave with the frame, not its read log.
        let hits = self.acquired.into_inner().expect("frame values").hits;
        statistics.hits += hits;
        statistics.resident_hits += hits;
        self.reads.into_inner().expect("query reads")
    }
}

/// Query results and named outputs already observed by this execution.
/// Inputs cannot change during an operation, so a second read can reuse both the value and
/// the dependency recorded by the first read.
/// Each query and parallel branch has its own table: sharing the operation's decoded values
/// must never let a new consumer skip recording its dependency.
#[derive(Default)]
struct Acquired {
    outputs: HashMap<Address, HashMap<Vec<u8>, Data>>,
    queries: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    hits: u64,
}

/// The information gathered while executing a query, or one parallel part of that query.
///
/// Dependencies say which answers this execution relied on. Outputs hold additional values it
/// produced with [`db.output`](super::Db::output), awaiting successful completion of the whole
/// query. Durability and persistence summarize whether and when these dependencies can be reused.
/// This data can leave its frame, which is how a completed parallel branch contributes its work.
pub(crate) struct Reads {
    pub(crate) dependencies: Vec<Dependency>,
    // Keep only the first read of each dependency without losing its place in the sequence.
    seen: HashSet<Dependency>,
    pub(crate) outputs: BTreeMap<Slot, Value>,
    // One frequently changing or request-only dependency restricts the whole producer.
    pub(crate) durability: Durability,
    pub(crate) lifetime: Lifetime,
}

impl Default for Reads {
    fn default() -> Self {
        // With no reads, no external change can affect the answer. Each recorded dependency
        // can lower durability or prevent persistence, never make the result less restricted.
        Self {
            dependencies: Vec::new(),
            seen: HashSet::new(),
            outputs: BTreeMap::new(),
            durability: Durability::High,
            lifetime: Lifetime::Persistent,
        }
    }
}

impl Reads {
    /// Remember the first occurrence of each read, preserving execution order.
    ///
    /// For `if enabled(db)? { left(db)? } else { right(db)? }`, the condition must come first.
    /// If it later changes, validation stops there and reruns the query instead of checking a
    /// branch the query might no longer use. Repeated reads need no second validation entry.
    fn depend(&mut self, dependency: Dependency, durability: Durability, lifetime: Lifetime) {
        // The result is only as reusable as its least reusable dependency. This also carries
        // restrictions through child queries without copying their own dependency lists here.
        self.durability = self.durability.min(durability);
        self.lifetime = self.lifetime.min(lifetime);
        if self.seen.insert(dependency.clone()) {
            self.dependencies.push(dependency);
        }
    }

    /// Add a completed parallel branch's reads and outputs to its owning query.
    /// The batch calls this in input order after joining workers, so the saved read order is
    /// stable even if workers finish in a different order. Shared reads are deduplicated as usual.
    pub(crate) fn merge(&mut self, branch: Self) -> Result<()> {
        for dependency in branch.dependencies {
            self.depend(dependency, branch.durability, branch.lifetime);
        }
        // Outputs emitted directly by callbacks belong to the same producer. Their slots must
        // be unique across branches too; nested queries keep their outputs in their own frames.
        for (slot, value) in branch.outputs {
            anyhow::ensure!(
                self.outputs.insert(slot, value).is_none(),
                "query emitted an output slot twice"
            );
        }
        Ok(())
    }
}
