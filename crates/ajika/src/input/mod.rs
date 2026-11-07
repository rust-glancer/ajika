//! Typed external values that queries can depend on.
//!
//! An [`Inputs`] snapshot owns the values for one input world. The host edits a private copy and
//! installs it in the database; readers of the previous snapshot keep their previous values.
//! A lazy initializer may fill an unseen key, but cannot replace an observation already made.
//! This lets file discovery happen inside a query without making ordinary input updates possible
//! there. The application still decides how to acquire a file and whether its bytes are current.

pub(crate) mod registry;
mod table;

use std::{
    any::TypeId,
    borrow::Borrow,
    collections::{HashMap, HashSet},
    fmt,
    hash::Hash,
    marker::PhantomData,
    sync::{Arc, RwLock},
};

use self::table::{ErasedTable, Table, Value};
use crate::{Durability, Lifetime, Persist, Result, Stamp};

/// One field of a keyed input declaration. Prefer [`#[ajika::input]`](macro@crate::input), which
/// generates this contract and typed getters/setters for each field of an ordinary struct.
///
/// The key and name identify the field across restarts. Values themselves need not be serializable:
/// a lazy source entry can identify its contents using an existing fingerprint instead. Its stamp
/// must cover everything queries can observe; mutable residency bookkeeping is not logical data.
pub trait Input: 'static {
    type Key: Persist + Clone + Eq + Hash + Send + Sync + 'static;
    type Value: Clone + Send + Sync + 'static;
    const NAME: &'static str;

    fn stamp(value: &Self::Value) -> Result<Stamp>;

    /// Acquire an unseen key. Initializers perform external acquisition, not query computation.
    /// Concurrent acquisitions may race; all readers receive the first value installed in the
    /// snapshot. No runtime lock is held while this function performs I/O.
    fn initialize(_key: &Self::Key) -> Result<Self::Value> {
        Err(crate::UnavailableData::Input { name: Self::NAME }.into())
    }

    fn durability(_value: &Self::Value) -> Durability {
        Durability::Low
    }

    fn lifetime(_value: &Self::Value) -> Lifetime {
        Lifetime::Persistent
    }
}

/// Select a declared field for host-side inspection and maintenance. Generated declarations
/// expose these selectors as constants, for example `File::SOURCE`. Query code normally uses
/// the corresponding `file.source(db)` getter instead.
pub struct Field<T: Input>(PhantomData<fn() -> T>);

impl<T: Input> Copy for Field<T> {}

impl<T: Input> Clone for Field<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Input> Field<T> {
    #[doc(hidden)]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T: Input> Default for Field<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// What a captured input proves about its value. Stored alongside the value so repeated reads
/// and dependency validation need neither another fingerprint nor another acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) stamp: Stamp,
    pub(crate) durability: Durability,
    pub(crate) lifetime: Lifetime,
}

/// A shareable collection of captured input values. Cloning shares the same observations; use
/// [`edit`](Self::edit) to produce a changed snapshot. Only changed field tables are copied, and
/// their values follow the declaration's normal `Clone` semantics (usually an `Arc` for large data).
///
/// [`overlay`](Self::overlay) starts a sparse view over a saved snapshot. This is useful for editor
/// buffers: the view owns changed files while sharing all saved inputs. Build successive editor
/// views from the saved snapshot, rather than chaining a new view onto every keystroke.
#[derive(Clone, Default)]
pub struct Inputs {
    tables: Arc<RwLock<HashMap<TypeId, Arc<dyn ErasedTable>>>>,
    base: Option<Arc<Inputs>>,
}

impl fmt::Debug for Inputs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inputs")
            .field("fields", &self.tables.read().expect("input tables").len())
            .field("overlay", &self.base.is_some())
            .finish()
    }
}

impl Inputs {
    /// Prepare changes without exposing a partial batch. An error (or unwinding) drops the private
    /// changes. The original snapshot remains usable, including its previously captured lazy keys.
    pub fn edit(&self, edit: impl FnOnce(&mut InputUpdate) -> Result<()>) -> Result<Self> {
        let mut update = InputUpdate {
            inputs: Self {
                tables: Arc::new(RwLock::new(
                    self.tables.read().expect("input tables").clone(),
                )),
                base: self.base.clone(),
            },
            changed: HashSet::new(),
        };
        edit(&mut update)?;
        Ok(update.inputs)
    }

    /// Create a sparse view over these inputs, initially with no local overrides.
    /// For example, an editor can keep the saved view as a base and prepare draft text with
    /// [`edit`](Self::edit). Other captured values stay shared. The view becomes active when
    /// installed with [`Database::replace_inputs`](crate::Database::replace_inputs).
    /// Pair draft values with [`Lifetime::Session`] to keep their query results in memory.
    pub fn overlay(&self) -> Self {
        Self {
            base: Some(Arc::new(self.clone())),
            ..Self::default()
        }
    }

    /// Keep this view's local fields while replacing its inherited inputs. A host can retain
    /// Cargo configuration and switch between saved sources and a sparse editor view.
    pub fn with_base(&self, base: &Self) -> Self {
        Self {
            tables: self.tables.clone(),
            base: Some(Arc::new(base.clone())),
        }
    }

    /// Host-side acquisition. Query code uses generated getters (or [`Db::input`](crate::Db::input))
    /// instead so Ajika records a dependency. This method is also useful during file discovery
    /// before the host installs the snapshot in a database.
    pub fn get<T: Input>(&self, _: Field<T>, key: &T::Key) -> Result<T::Value> {
        Ok(self.read::<T>(key)?.value)
    }

    /// Inspect a captured value without triggering its initializer.
    pub fn peek<T: Input, K: Eq + Hash + ?Sized>(&self, _: Field<T>, key: &K) -> Option<T::Value>
    where
        T::Key: Borrow<K>,
    {
        self.lookup::<T, K, _>(key, |value| value.value.clone())
            .flatten()
    }

    /// Enumerate captured values for host maintenance, for example dropping reloadable source
    /// buffers or validating filesystem observations before publishing a project. No initializer
    /// runs, and the returned list does not hold runtime locks while the host visits its entries.
    pub fn entries<T: Input>(&self, _: Field<T>) -> Vec<(T::Key, T::Value)> {
        self.values::<T>()
            .into_iter()
            .filter_map(|(key, value)| value.map(|v| (key, v.value)))
            .collect()
    }

    /// Approximate resident table allocation, excluding allocations owned by keys and values.
    /// Hosts with structural memory reports can account for those through [`entries`](Self::entries).
    pub fn table_bytes(&self) -> usize {
        let mut seen = HashSet::new();
        let mut bytes = 0;
        let mut view = Some(self);
        while let Some(inputs) = view {
            for table in inputs.tables.read().expect("input tables").values() {
                if seen.insert(Arc::as_ptr(table) as *const () as usize) {
                    bytes += table.bytes();
                }
            }
            view = inputs.base.as_deref();
        }
        bytes
    }

    /// Repack allocations without changing logical inputs. All snapshots sharing the table see
    /// the new representation. Keys, fingerprints and retention policies must remain identical.
    /// This is maintenance for applications that release allocator pages at idle boundaries;
    /// semantic changes belong in [`edit`](Self::edit).
    pub fn relocate<T: Input>(
        &self,
        _: Field<T>,
        mut relocate: impl FnMut(&T::Key, &T::Value) -> (T::Key, T::Value),
    ) -> Result<()> {
        if let Some(table) = self.table::<T>() {
            let mut values = table.values.write().expect("input values");
            let mut relocated = HashMap::with_capacity(values.len());
            for (key, value) in values.iter() {
                let value = match value {
                    Some(value) => {
                        let (new_key, new_value) = relocate(key, &value.value);
                        let new_value = Value::<T>::new(new_value)?;
                        if !(new_key == *key && new_value.record == value.record) {
                            return Err(crate::Error::contract(format!(
                                "relocation changed input {}",
                                T::NAME
                            )));
                        }
                        relocated.insert(new_key, Some(new_value));
                        continue;
                    }
                    None => None,
                };
                relocated.insert(key.clone(), value);
            }
            *values = relocated;
        }
        Ok(())
    }

    pub(crate) fn read<T: Input>(&self, key: &T::Key) -> Result<Value<T>> {
        if let Some(Some(value)) = self.lookup::<T, T::Key, _>(key, Clone::clone) {
            return Ok(value);
        }
        // A composed view (for example configuration over a source inventory) acquires new
        // observations in its base unless it owns an override table for this field. Both the
        // host and query validation then see the same late-discovered files.
        if self.table::<T>().is_none()
            && let Some(base) = &self.base
        {
            return base.read::<T>(key);
        }
        // Capture outside locks. A racing reader may have installed an observation in the
        // meantime, in which case use that same observation instead of our own capture.
        let value = Value::<T>::new(T::initialize(key)?)?;
        let table = self.ensure_table::<T>();
        let mut values = table.values.write().expect("input values");
        Ok(values
            .entry(key.clone())
            .or_default()
            .get_or_insert(value)
            .clone())
    }

    /// Validation only needs evidence, not another clone of a potentially large input value.
    pub(crate) fn record<T: Input>(&self, key: &T::Key) -> Result<Record> {
        match self
            .lookup::<T, T::Key, _>(key, |value| value.record)
            .flatten()
        {
            Some(record) => Ok(record),
            None => Ok(self.read::<T>(key)?.record),
        }
    }

    /// Distinguish an unseen key (`None`) from a forgotten observation (`Some(None)`). Forgetting
    /// stops lookup here: falling through would bring back the observation from the base.
    fn lookup<T: Input, K: Eq + Hash + ?Sized, R>(
        &self,
        key: &K,
        read: impl Fn(&Value<T>) -> R + Copy,
    ) -> Option<Option<R>>
    where
        T::Key: Borrow<K>,
    {
        if let Some(table) = self.table::<T>() {
            if let Some(value) = table.values.read().expect("input values").get(key) {
                return Some(value.as_ref().map(read));
            }
            if table.reset {
                return Some(None);
            }
        }
        self.base
            .as_ref()
            .and_then(|base| base.lookup::<T, K, R>(key, read))
    }

    fn values<T: Input>(&self) -> HashMap<T::Key, Option<Value<T>>> {
        let table = self.table::<T>();
        let table = table.as_ref();
        let mut values = if table.is_some_and(|t| t.reset) {
            HashMap::new()
        } else {
            self.base
                .as_ref()
                .map_or_else(HashMap::new, |base| base.values::<T>())
        };
        if let Some(table) = table {
            values.extend(table.values.read().expect("input values").clone());
        }
        values
    }

    fn table<T: Input>(&self) -> Option<Arc<Table<T>>> {
        self.tables
            .read()
            .expect("input tables")
            .get(&TypeId::of::<T>())
            .cloned()
            .map(Table::<T>::typed)
    }

    fn ensure_table<T: Input>(&self) -> Arc<Table<T>> {
        if let Some(table) = self.table::<T>() {
            return table;
        }
        Table::<T>::typed(
            self.tables
                .write()
                .expect("input tables")
                .entry(TypeId::of::<T>())
                .or_insert_with(|| Arc::new(Table::<T>::default()))
                .clone(),
        )
    }

    /// Find the highest class whose observations could differ. The common editor case compares
    /// only its sparse overrides; saved tables shared by both views need no scan.
    pub(crate) fn changed_since(&self, previous: &Self) -> Option<Durability> {
        if Arc::ptr_eq(&self.tables, &previous.tables) && self.same_base(previous) {
            return None;
        }
        let mut tables = self.all_tables();
        tables.extend(previous.all_tables());
        tables
            .values()
            .filter_map(|table| table.changed(self, previous))
            .max()
    }

    /// Effective layers for one field, newest first. A reset masks all inherited observations.
    fn layers<T: Input>(&self) -> Vec<Arc<Table<T>>> {
        let mut layers = Vec::new();
        let mut view = Some(self);
        while let Some(inputs) = view {
            if let Some(table) = inputs.table::<T>() {
                let reset = table.reset;
                layers.push(table);
                if reset {
                    break;
                }
            }
            view = inputs.base.as_deref();
        }
        layers
    }

    fn same_base(&self, other: &Self) -> bool {
        match (&self.base, &other.base) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(&a.tables, &b.tables) && a.same_base(b),
            _ => false,
        }
    }

    fn all_tables(&self) -> HashMap<TypeId, Arc<dyn ErasedTable>> {
        let mut tables = self
            .base
            .as_ref()
            .map_or_else(HashMap::new, |base| base.all_tables());
        tables.extend(self.tables.read().expect("input tables").clone());
        tables
    }
}

/// The host's private batch of input changes. Generated setters write here, so neither a query
/// nor a shared snapshot offers an API for replacing a value in place.
pub struct InputUpdate {
    inputs: Inputs,
    changed: HashSet<TypeId>,
}

impl InputUpdate {
    pub fn set<T: Input>(&mut self, _: Field<T>, key: T::Key, value: T::Value) -> Result<()> {
        let value = Value::<T>::new(value)?;
        let table = self.table::<T>();
        table
            .values
            .write()
            .expect("input values")
            .insert(key, Some(value));
        Ok(())
    }

    /// Forget one observation so the next read acquires it again. This also hides an inherited
    /// observation in an overlay. A declaration without an initializer must be supplied again.
    pub fn forget<T: Input>(&mut self, _: Field<T>, key: T::Key) {
        let table = self.table::<T>();
        let mut values = table.values.write().expect("input values");
        // Mask the base even if it has not captured this key yet: a later base acquisition
        // must not undo this view's request to reacquire it independently.
        if self.inputs.base.is_some() && !table.reset {
            values.insert(key, None);
        } else {
            values.remove(&key);
        }
    }

    /// Forget all observations of a field, such as existence probes after a filesystem change
    /// batch. Later reads run its initializer again; this does not make the field permanently absent.
    pub fn forget_all<T: Input>(&mut self, _: Field<T>) {
        self.inputs.tables.write().expect("input tables").insert(
            TypeId::of::<T>(),
            Arc::new(Table::<T> {
                values: RwLock::default(),
                reset: true,
            }),
        );
        self.changed.insert(TypeId::of::<T>());
    }

    fn table<T: Input>(&mut self) -> Arc<Table<T>> {
        // Copy a field table on its first write in this batch. Later setters reuse that private
        // table, so a batch of file updates pays for one copy rather than one copy per file.
        if self.changed.insert(TypeId::of::<T>()) {
            let table = self.inputs.table::<T>().map_or_else(
                || Arc::new(Table::<T>::default()) as Arc<dyn ErasedTable>,
                |table| table.fork(),
            );
            self.inputs
                .tables
                .write()
                .expect("input tables")
                .insert(TypeId::of::<T>(), table);
        }
        self.inputs.ensure_table::<T>()
    }
}
