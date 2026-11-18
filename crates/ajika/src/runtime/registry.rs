//! Saved addresses carry a cache-local family number and an encoded key. The interner
//! remembers which declaration name each number belongs to. The registry connects those
//! numbers to the query and input implementations linked into this process.

use std::{
    any::TypeId,
    collections::HashMap,
    sync::{Arc, RwLock, RwLockReadGuard},
};

use super::{Db, cache::Memo, record::Address};
use crate::{
    __private::Registration,
    Input, Interner, Query, Result, Services, codec,
    input::{
        Record as InputRecord,
        registry::{InputRegistry, ReadInput},
    },
    interner::{FamilyId, FamilyKind},
};

/// The bool requests the primary value. Dependency validation can ask for the version alone
/// and leave the payload unloaded.
type Replay<S> = fn(&Db<'_, S>, &[u8], bool) -> Result<Arc<Memo>>;

struct QueryEntry<S: Services> {
    identity: TypeId,
    replay: Replay<S>,
}

enum Provider<S: Services> {
    Query(Replay<S>),
    Input(ReadInput),
    Unregistered,
}

struct Family<S: Services> {
    name: Arc<str>,
    provider: Provider<S>,
}

struct Bindings<S: Services> {
    // Typed calls only enter the shared interner on first use of a declaration.
    typed: HashMap<(FamilyKind, TypeId), FamilyId>,
    // Dynamic replay goes straight from the saved number to the handler.
    families: Vec<Family<S>>,
}

impl<S: Services> Bindings<S> {
    fn provider(&self, id: FamilyId) -> Option<&Provider<S>> {
        self.families
            .get(usize::from(id.0))
            .map(|family| &family.provider)
    }
}

pub(crate) struct Registry<S: Services> {
    queries: HashMap<&'static str, QueryEntry<S>>,
    inputs: InputRegistry,
    interner: Interner,
    bindings: RwLock<Bindings<S>>,
}

impl<S: Services> Registry<S> {
    pub(crate) fn linked(interner: &Interner) -> Self {
        let mut registry = Self {
            queries: HashMap::new(),
            inputs: InputRegistry::linked(),
            interner: interner.clone(),
            bindings: RwLock::new(Bindings {
                typed: HashMap::new(),
                families: Vec::new(),
            }),
        };
        // An installer accepts only a registry for its own Services type.
        for declaration in inventory::iter::<Registration> {
            (declaration.install)(&mut registry);
        }

        // A parent's saved dependency can be the first use of a family after restart.
        // Restore the handlers in the cache's order before any typed calls arrive.
        let families = interner
            .family_names()
            .into_iter()
            .map(|(kind, name)| registry.bind(kind, name))
            .collect();
        registry
            .bindings
            .get_mut()
            .expect("family bindings")
            .families = families;
        registry
    }

    /// Re-registering Q is harmless, but a different query using its saved name
    /// would make old dependencies ambiguous.
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

    fn bind(&self, kind: FamilyKind, name: Arc<str>) -> Family<S> {
        // A handwritten provider can be called directly without being registered. Keep its
        // name for diagnostics, but only a linked declaration is allowed to replay a saved call.
        let provider = match kind {
            FamilyKind::Query => self
                .queries
                .get(name.as_ref())
                .map(|entry| Provider::Query(entry.replay)),
            FamilyKind::Input => self.inputs.reader(&name).map(Provider::Input),
        }
        .unwrap_or(Provider::Unregistered);
        Family { name, provider }
    }

    fn bindings(&self) -> RwLockReadGuard<'_, Bindings<S>> {
        self.bindings.read().expect("family bindings")
    }

    fn family_id<T: 'static>(&self, kind: FamilyKind, name: &'static str) -> Result<FamilyId> {
        let key = (kind, TypeId::of::<T>());
        if let Some(id) = self.bindings().typed.get(&key) {
            return Ok(*id);
        }
        let mut bindings = self.bindings.write().expect("family bindings");
        // Another worker may have bound this type while we waited for the write lock.
        if let Some(id) = bindings.typed.get(&key) {
            return Ok(*id);
        }
        let id = self.interner.intern_family(kind, name)?;
        // Restored IDs already have a handler. A new ID is the next slot in the table.
        if usize::from(id.0) == bindings.families.len() {
            bindings.families.push(self.bind(kind, Arc::from(name)));
        }
        bindings.typed.insert(key, id);
        Ok(id)
    }

    pub(super) fn query_family<Q: Query<Services = S>>(&self) -> Result<FamilyId> {
        self.family_id::<Q>(FamilyKind::Query, Q::NAME)
    }

    pub(super) fn input_family<T: Input>(&self) -> Result<FamilyId> {
        self.family_id::<T>(FamilyKind::Input, T::NAME)
    }

    pub(super) fn family_name(&self, id: FamilyId) -> Arc<str> {
        self.bindings()
            .families
            .get(usize::from(id.0))
            .map(|family| family.name.clone())
            .unwrap_or_else(|| format!("family #{}", id.0).into())
    }

    /// Evaluate a saved call. A missing registration makes the parent rerun, and an error
    /// from a registered query fails the operation. Copy the function pointer before calling
    /// it because a nested query may need to register another family.
    pub(super) fn replay(
        &self,
        db: &Db<'_, S>,
        address: &Address,
        value: bool,
    ) -> Result<Option<Arc<Memo>>> {
        let replay = match self.bindings().provider(address.family) {
            Some(Provider::Query(replay)) => *replay,
            _ => return Ok(None),
        };
        replay(db, &address.key, value).map(Some)
    }

    /// Read the input's current stamp and reuse policy. A missing registration means the
    /// old dependency can no longer establish the parent's validity.
    pub(super) fn stamp(&self, db: &Db<'_, S>, address: &Address) -> Result<Option<InputRecord>> {
        let read = match self.bindings().provider(address.family) {
            Some(Provider::Input(read)) => *read,
            _ => return Ok(None),
        };
        read(&db.operation.database.inputs, &address.key, db).map(Some)
    }
}
