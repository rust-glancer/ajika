//! A cache uses one numbering space for query and input families. The tag keeps a query
//! and an input with the same declared name distinct. The name is saved once in the interner.

use std::sync::Arc;

use wincode::{SchemaRead, SchemaWrite};

use super::Interner;
use crate::{Error, Result};

pub(super) const NAMESPACE: &str = "\0families";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SchemaRead, SchemaWrite)]
pub(crate) struct FamilyId(pub(crate) u16);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FamilyKind {
    Query,
    Input,
}

impl FamilyKind {
    pub(super) fn decode(key: &[u8]) -> Result<(Self, &str)> {
        let (kind, name) = match key.split_first() {
            Some((b'Q', name)) => (Self::Query, name),
            Some((b'I', name)) => (Self::Input, name),
            _ => return Err(Error::invalid_cache("invalid family kind")),
        };
        let name = str::from_utf8(name).map_err(|_| Error::invalid_cache("invalid family name"))?;
        Ok((kind, name))
    }
}

impl Interner {
    pub(crate) fn intern_family(&self, kind: FamilyKind, name: &str) -> Result<FamilyId> {
        let mut key = Vec::with_capacity(1 + name.len());
        key.push(match kind {
            FamilyKind::Query => b'Q',
            FamilyKind::Input => b'I',
        });
        key.extend_from_slice(name.as_bytes());
        let id = self.intern(NAMESPACE.to_owned(), &key)?;
        Ok(FamilyId(u16::try_from(id).expect("family IDs fit in u16")))
    }

    /// Reconnect the saved numbering to the declarations linked into this process.
    /// The vector order is the assignment order, including families that are no longer linked.
    pub(crate) fn family_names(&self) -> Vec<(FamilyKind, Arc<str>)> {
        let state = self.state.lock().expect("interner state");
        let Some(entries) = state.namespaces.get(NAMESPACE) else {
            return Vec::new();
        };
        entries
            .keys
            .iter()
            .map(|key| {
                let (kind, name) = FamilyKind::decode(key).expect("validated family assignment");
                (kind, Arc::from(name))
            })
            .collect()
    }
}
