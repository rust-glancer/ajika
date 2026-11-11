//! Running a batch of query-aware work concurrently inside one operation.
//!
//! [`Db::parallel_map`] lets the host or a query divide work over a list of items. Callbacks share
//! the operation's inputs and remembered query answers, while reads made by each callback still
//! become dependencies of the calling query. This module owns batch scheduling and joining;
//! the normal evaluator and coordinator handle any query calls made inside the callbacks.

use std::{
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use super::{Db, coordination::Coordination, frame::Frame};
use crate::{Error, Result, Services};

/// One extra thread taken from the operation's shared worker budget. Returning it on drop lets
/// later nested batches use that capacity, including when the worker exits by unwinding.
struct Permit<'a>(&'a AtomicUsize);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

/// A callback-lifetime guard that tells other workers to stop if this worker panics.
/// Its scope includes application code between query calls, where no query claim is active.
struct Worker<'a>(&'a Coordination);

impl Drop for Worker<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            // A callback can panic between query calls, with no claim guard on its
            // stack. Signal other workers before the thread scope starts joining them.
            self.0
                .fail(Error::invariant("parallel query callback panicked"));
        }
    }
}

impl<S: Services> Db<'_, S> {
    /// Apply a callback to independent items concurrently, such as calling `words` for each file.
    /// Each callback receives a [`Db`] for the same operation, so nested queries share answers as
    /// usual. Return values stay in input order even when callbacks finish in a different order.
    ///
    /// Input and child-query reads in callbacks are dependencies of the calling query.
    /// All workers finish before this returns, including on an error.
    ///
    /// Use this for coarse work. The calling thread participates, and nested batches share the
    /// limit configured by [`Database::set_parallelism`](crate::Database::set_parallelism).
    pub fn parallel_map<T: Sync, R: Send>(
        &self,
        items: &[T],
        function: impl Fn(&Db<'_, S>, &T) -> Result<R> + Sync,
    ) -> Result<Vec<R>> {
        self.handle(|| {
            self.check_cancelled()?;
            // Workers take the next available item, but save its answer and reads at the input
            // index. That separates load balancing from the order seen by callers and validation.
            let next = AtomicUsize::new(0);
            let results = Mutex::new((0..items.len()).map(|_| None).collect::<Vec<_>>());
            // A host may install a scoped subscriber rather than a process-wide one. Our own
            // worker threads must use that same filter and destination for query diagnostics.
            let dispatcher = tracing::dispatcher::get_default(Clone::clone);
            let worker = || {
                let _dispatcher = tracing::dispatcher::set_default(&dispatcher);
                let _worker = Worker(&self.operation.database.cancellation.inner);
                loop {
                    // The coordinator remembers why work stopped. Leave the remaining items
                    // untouched instead of creating frames just to report the same error again.
                    if self.check_cancelled().is_err() {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    // A branch is still part of the calling query, not a new query producer.
                    // Keep its owner for cycle detection, but record reads separately so their
                    // eventual order does not depend on which worker happened to finish first.
                    let frame = Frame::new(self.frame.owner.clone());
                    let branch = Db {
                        operation: self.operation,
                        frame: &frame,
                    };
                    let result = branch.handle(|| {
                        branch.check_cancelled()?;
                        function(&branch, item)
                    });
                    let failed = result.is_err();
                    results.lock().expect("parallel results")[index] =
                        Some((result, frame.finish(&mut self.operation.stats())));
                    if failed {
                        break;
                    }
                }
            };
            thread::scope(|scope| {
                // The caller always does work too. Nested batches share the spare permits,
                // so exhausting the thread budget simply leaves this batch on its caller.
                for _ in 1..items.len() {
                    if self.check_cancelled().is_err() {
                        break;
                    }
                    let acquired = self
                        .operation
                        .spare
                        .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                            count.checked_sub(1)
                        })
                        .is_ok();
                    if !acquired {
                        break;
                    }
                    let permit = Permit(&self.operation.spare);
                    let worker = &worker;
                    scope.spawn(move || {
                        let _permit = permit;
                        worker();
                    });
                }
                worker();
            });

            // A stopped batch can leave unvisited items. Prefer completed branch errors so
            // callback context survives, then check for cancellation or a caught query failure.
            let results = results.into_inner().expect("parallel results");
            if let Some(error) = results
                .iter()
                .flatten()
                .find_map(|(result, _)| result.as_ref().err())
            {
                return Err(error.clone());
            }
            self.check_cancelled()?;

            let mut values = Vec::with_capacity(items.len());
            // Each branch's reads stay in execution order, and branches stay in input order.
            // Later validation can therefore stop at a changed read before replaying reads
            // that depended on it, just as it would for a sequential provider.
            for result in results {
                let (result, reads) = result.expect("successful batch visited every item");
                values.push(result?);
                self.frame.reads.lock().expect("query reads").merge(reads)?;
            }
            self.check_cancelled()?;
            Ok(values)
        })
    }
}
