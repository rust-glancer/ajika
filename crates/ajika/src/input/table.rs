//! Storage for one field's keyed values. Type erasure only selects the field table; values and
//! keys remain typed inside it. No input payload encoding or decoding is involved in a read.

use std::{
    any::Any,
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};

use super::{Input, Inputs, Record};
use crate::{Durability, Result};

/// An acquired value and the evidence queries use to recognize it. Capture computes that
/// evidence once; validation can inspect the record without cloning the value.
pub(crate) struct Value<T: Input> {
    pub(crate) value: T::Value,
    pub(crate) record: Record,
}

impl<T: Input> Clone for Value<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            record: self.record,
        }
    }
}

impl<T: Input> Value<T> {
    pub(super) fn new(value: T::Value) -> Result<Self> {
        let record = Record {
            stamp: T::stamp(&value)?,
            durability: T::durability(&value),
            lifetime: T::lifetime(&value),
        };
        Ok(Self { value, record })
    }
}

pub(super) trait ErasedTable: Any + Send + Sync {
    fn fork(&self) -> Arc<dyn ErasedTable>;

    fn changed(&self, current: &Inputs, previous: &Inputs) -> Option<Durability>;

    fn bytes(&self) -> usize;
}

/// Captures and overrides for one input field. A `None` entry hides that key in the base so
/// its next read can acquire a fresh observation. `reset` hides the whole inherited field,
/// including keys that have not been captured yet. Both masks survive copying a snapshot.
pub(super) struct Table<T: Input> {
    pub(super) values: RwLock<HashMap<T::Key, Option<Value<T>>>>,
    pub(super) reset: bool,
}

impl<T: Input> Table<T> {
    pub(super) fn typed(table: Arc<dyn ErasedTable>) -> Arc<Self> {
        let table: Arc<dyn Any + Send + Sync> = table;
        table.downcast().expect("input table selected by its type")
    }
}

impl<T: Input> Default for Table<T> {
    fn default() -> Self {
        Self {
            values: RwLock::default(),
            reset: false,
        }
    }
}

impl<T: Input> ErasedTable for Table<T> {
    fn fork(&self) -> Arc<dyn ErasedTable> {
        Arc::new(Self {
            values: RwLock::new(self.values.read().expect("input values").clone()),
            reset: self.reset,
        })
    }

    fn changed(&self, current: &Inputs, previous: &Inputs) -> Option<Durability> {
        let mut next_layers = current.layers::<T>();
        let mut old_layers = previous.layers::<T>();
        // An editor view shares its saved tail with the preceding view. Only keys in changed
        // layers can differ; comparing a keystroke must not clone every saved source value.
        while next_layers
            .last()
            .zip(old_layers.last())
            .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
        {
            next_layers.pop();
            old_layers.pop();
        }
        let mut keys = HashSet::new();
        for layer in next_layers.iter().chain(&old_layers) {
            keys.extend(layer.values.read().expect("input values").keys().cloned());
        }
        let mut changed = None;
        for key in keys {
            let next = current.lookup::<T, T::Key, _>(&key, |v| v.record).flatten();
            let old = previous
                .lookup::<T, T::Key, _>(&key, |v| v.record)
                .flatten();
            if next != old {
                changed = changed
                    .max(next.map(|v| v.durability))
                    .max(old.map(|v| v.durability));
            }
        }
        changed
    }

    fn bytes(&self) -> usize {
        size_of::<Self>()
            + self.values.read().expect("input values").capacity()
                * (size_of::<(T::Key, Option<Value<T>>)>() + 1)
    }
}
