//! Resolve saved input addresses independently of the query's host-service type. Input values
//! belong to typed tables, so validating them does not need an application provider object.

use std::{any::TypeId, collections::HashMap};

use super::{Input, InputInit, Inputs, Record};
use crate::{__private::Registration, Result, codec};

struct Entry {
    identity: TypeId,
    read: fn(&Inputs, &[u8], &dyn InputInit) -> Result<Record>,
}

pub(crate) struct Registry(HashMap<&'static str, Entry>);

impl Registry {
    pub(crate) fn linked() -> Self {
        let mut registry = Self(HashMap::new());
        for declaration in inventory::iter::<Registration> {
            (declaration.install)(&mut registry);
        }
        registry
    }

    pub(crate) fn input<T: Input>(&mut self) {
        let entry = Entry {
            identity: TypeId::of::<T>(),
            read: |inputs, key, cx| inputs.record::<T>(&codec::decode(key)?, cx),
        };
        if let Some(previous) = self.0.insert(T::NAME, entry) {
            assert_eq!(
                previous.identity,
                TypeId::of::<T>(),
                "duplicate input name: {}",
                T::NAME
            );
        }
    }

    pub(crate) fn stamp(
        &self,
        inputs: &Inputs,
        name: &str,
        key: &[u8],
        cx: &dyn InputInit,
    ) -> Result<Option<Record>> {
        self.0
            .get(name)
            .map(|entry| (entry.read)(inputs, key, cx))
            .transpose()
    }
}
