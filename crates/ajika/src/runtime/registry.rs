//! Finding the Rust implementation for a query or input named in a saved dependency.
//!
//! Ordinary calls know their query type at compile time. Saved dependencies instead contain a
//! stable name and encoded key. The registry connects those names to callable implementations,
//! allowing the evaluator to check old dependencies after reopening the database. Declarations
//! supply registrations; the runtime uses [`replay`](Registry::replay) and
//! [`stamp`](Registry::stamp) to revisit the saved reads.

use std::{any::TypeId, collections::HashMap, sync::Arc};

use super::{Db, cache::Memo, record::Address};
use crate::{
    __private::Registration, Query, Result, Services, codec, input::Record as InputRecord,
};

/// A query call with its key still encoded. The bool requests the primary value as well as
/// a valid output version; false allows dependency validation to leave the payload unloaded.
type Replay<S> = fn(&Db<'_, S>, &[u8], bool) -> Result<Arc<Memo>>;

/// A saved name's query implementation, with a type identity for detecting name collisions.
struct QueryEntry<S: Services> {
    identity: TypeId,
    replay: Replay<S>,
}

/// All query and input implementations available for one database's host-service type.
///
/// For a saved `words("lib.rs")` dependency, look up the name `words`, decode `"lib.rs"` using that
/// query's key type, then enter the normal evaluator. The stored name is stable across restarts.
/// [`TypeId`] values only check that two different Rust implementations did not claim that name in
/// this process; they are not part of persistent identity.
pub(crate) struct Registry<S: Services> {
    queries: HashMap<&'static str, QueryEntry<S>>,
    inputs: crate::input::registry::Registry,
}

impl<S: Services> Registry<S> {
    /// Gather declarations from the linked program once when constructing a database.
    /// Query calls can then look up names directly instead of scanning registrations each time.
    pub(crate) fn linked() -> Self {
        let mut registry = Self {
            queries: HashMap::new(),
            inputs: crate::input::registry::Registry::linked(),
        };
        // Inventory contains declarations from all linked crates and service types. Each installer
        // accepts only a registry for its own Services type, so unrelated databases stay separate.
        for declaration in inventory::iter::<Registration> {
            (declaration.install)(&mut registry);
        }
        registry
    }

    /// Teach replay how to decode Q's keys and enter its evaluator. Re-registering Q is harmless,
    /// but a different query using Q's saved name would make old dependencies ambiguous.
    pub(crate) fn query<Q: Query<Services = S>>(&mut self) {
        let entry = QueryEntry {
            identity: TypeId::of::<Q>(),
            replay: |db, key, value| db.ensure::<Q>(codec::decode(key)?, value),
        };
        if let Some(previous) = self.queries.insert(Q::NAME, entry) {
            assert_eq!(
                previous.identity,
                TypeId::of::<Q>(),
                "duplicate query name: {}",
                Q::NAME
            );
        }
    }

    /// Evaluate the query described by a saved address, with the same reuse rules as a normal call.
    /// `value` says whether the caller needs its primary value loaded or only its current version.
    ///
    /// None means this program has no registration for the saved name. When checking a saved
    /// parent's dependencies, this makes the parent run again because the old read cannot be
    /// verified. An error from a registered query instead fails the operation; it is not treated
    /// as a missing cache entry.
    pub(crate) fn replay(
        &self,
        db: &Db<'_, S>,
        address: &Address,
        value: bool,
    ) -> Result<Option<Arc<Memo>>> {
        self.queries
            .get(address.family.as_str())
            .map(|entry| (entry.replay)(db, &address.key, value))
            .transpose()
    }

    /// Read the current stamp and reuse policy for a saved input address. None means its declaration
    /// is no longer registered, so the old dependency cannot establish the parent's validity.
    pub(crate) fn stamp(&self, db: &Db<'_, S>, address: &Address) -> Result<Option<InputRecord>> {
        self.inputs.stamp(
            &db.operation.database.inputs,
            &address.family,
            &address.key,
            db,
        )
    }
}
