//! Shared query working state during one operation.
//!
//! An operation can ask for `words("lib.rs")` from several callers or workers. [`Memos`]
//! gives them one [`Memo`] so they share validation, computation and decoded values.
//! The backend owns completed records between operations; these working objects do not
//! keep their keys, output metadata or locks resident while the database is idle.

use std::{
    any::{Any, TypeId},
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard, RwLock},
};

use super::{
    record::{Address, QueryId, Record, RecordHeader, Slot},
    value::Value,
};
use crate::{Query, Result};

/// One query call being used by the current operation, such as `words("lib.rs")`.
///
/// Callers with the same query and key share this object. It first holds a loaded record
/// or a new computation, then the operation checkpoints any changes. Its decoded values
/// remain available to other callers until the operation finishes. The next operation
/// creates a new memo and recovers the record and validity proof from the backend.
pub(crate) struct Memo {
    /// Keep the declaration's static name available to claims without cloning the replay address.
    pub(crate) query_name: &'static str,
    pub(crate) address: Address,
    pub(crate) id: QueryId,
    pub(crate) state: Mutex<State>,
}

impl Memo {
    /// Borrow the remembered result and validity information for a short read or update.
    /// This lock does not reserve the computation: the evaluator takes a separate query claim
    /// before doing work, so provider calls and I/O need not run under the state lock.
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("query memo")
    }
}

/// The remembered answer and reuse information for a query call, updated as the call is served.
///
/// A new memo starts empty. Loading a saved record supplies its previous dependencies and
/// output versions. Validation marks them as checked for this input revision; execution may
/// replace them and supply new values. The operation can use those new values immediately,
/// before the checkpoint saves them to the backend.
///
/// Keep validity, loaded data, and pending writes separate. A valid answer may have no loaded
/// value, and a newly computed valid answer may still need saving. After an operation,
/// the entire state is released. Completed records and validity live in the backend.
#[derive(Default)]
pub(crate) struct State {
    /// Output versions and locations used by callers in this operation.
    pub(crate) header: Option<Arc<RecordHeader>>,
    /// Distinguish "not looked up yet" from "looked up and absent" when [`record`](State::record)
    /// is None. A retained header alone does not tell us whether this operation loaded the full
    /// record.
    pub(crate) record_loaded: bool,
    /// The loaded or newly computed record that readers in this operation should see.
    pub(crate) record: Option<Arc<Record>>,
    /// Input revision at which the current header was proved valid. Zero means no such proof
    /// exists in this process; stored records cannot carry that proof across a restart.
    pub(crate) verified: u64,
    /// Loaded or newly computed Rust values, shared by callers until the operation ends.
    pub(crate) values: BTreeMap<Slot, Value>,
    /// Metadata or payloads need checkpointing. This can be true even when outputs compare
    /// equal, because the computation may have observed a different set of dependencies.
    pub(crate) dirty: bool,
}

impl State {
    /// Make this the record seen by subsequent calls in the operation. The retained header must
    /// point to this record's outputs too, or a fast read could see versions from an older result.
    /// This does not mark the query valid or saved; the caller knows which step produced the record.
    pub(crate) fn install_record(&mut self, record: Arc<Record>) {
        self.header = Some(record.header.clone());
        self.record = Some(record);
        self.record_loaded = true;
    }
}

type Table<Q> = Mutex<HashMap<<Q as Query>::Key, Arc<Memo>>>;

/// Query calls shared by the workers in one operation, indexed by their Rust keys.
/// For example, two workers asking for `words("lib.rs")` find one memo and share its
/// validation, computation and decoded value. The whole table leaves with the operation;
/// the backend keeps completed records and their small cross-operation validity proofs.
#[derive(Default)]
pub(crate) struct Memos(RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>);

impl Memos {
    /// Find or create the place where this call will keep its result. A new memo is empty;
    /// getting it does not read storage, validate dependencies, or execute the query.
    pub(crate) fn get<Q: Query>(&self, key: &Q::Key) -> Result<Arc<Memo>> {
        // Look up the query's typed table first. Creating a previously unseen family needs an
        // exclusive map lock, but calls to existing families can find their tables concurrently.
        let table = self
            .0
            .read()
            .expect("query families")
            .get(&TypeId::of::<Q>())
            .cloned();
        let table = match table {
            Some(table) => table,
            None => {
                let mut families = self.0.write().expect("query families");
                families
                    .entry(TypeId::of::<Q>())
                    .or_insert_with(|| Arc::new(Table::<Q>::default()))
                    .clone()
            }
        };
        let table = table.downcast::<Table<Q>>().expect("query family type");
        let mut entries = table.lock().expect("query keys");
        if let Some(memo) = entries.get(key) {
            return Ok(memo.clone());
        }
        // Encode the replay address only when creating a memo. Later lookups use the typed key.
        let address = Address::new(Q::NAME, key)?;
        let memo = Arc::new(Memo {
            query_name: Q::NAME,
            id: address.id(),
            address,
            state: Mutex::default(),
        });
        entries.insert(key.clone(), memo.clone());
        Ok(memo)
    }
}
