//! Holding results of different query types in the same runtime data structures.
//!
//! A memo can contain a query's return value and named outputs with unrelated Rust types. [`Value`]
//! gives the runtime one representation for all of them: a shared object plus the operations needed
//! to compare or store it. Query callers still receive their concrete [`Arc<T>`](Arc); only the
//! runtime needs to handle these objects without knowing T in advance.

use std::{any::Any, sync::Arc};

use crate::{Error, Persist, Result, codec};

pub(crate) type Data = Arc<dyn Any + Send + Sync>;

/// The encode, decode, and equality operations for one concrete result type.
///
/// Erasing [`Arc<T>`](Arc) to [`Data`] lets the runtime hold the value, but does not tell it how T
/// is serialized or compared. This table supplies those operations as function pointers with a
/// uniform signature. A persistent query or named output constructs it while its type is known.
/// Transient return values omit it because they require neither serialization nor comparison.
#[derive(Clone, Copy)]
pub(crate) struct Codec {
    pub(crate) size: fn(&Data) -> Result<u64>,
    pub(crate) decode: fn(&[u8]) -> Result<Data>,
    pub(crate) encode: fn(&Data, u64) -> Result<Vec<u8>>,
    pub(crate) equal: fn(&Data, &Data) -> Result<bool>,
}

impl Codec {
    /// Bind the operations to T before the value enters the untyped runtime maps.
    /// Each callback recovers T and invokes its ordinary Rust implementation; equality therefore
    /// uses [`PartialEq`] rather than comparing serialized bytes.
    pub(crate) fn of<T: Persist + PartialEq + Send + Sync + 'static>() -> Self {
        Self {
            size: |value| codec::encoded_size(value.downcast_ref::<T>().expect("query value type")),
            decode: |bytes| Ok(Arc::new(codec::decode::<T>(bytes)?)),
            encode: |value, length| {
                let value = value
                    .downcast_ref::<T>()
                    .ok_or_else(|| Error::invariant("query value type changed"))?;
                // The checkpoint already measured this value to form bounded batches. Reuse
                // that capacity instead of asking serialize() to walk it a second time.
                let mut bytes = Vec::with_capacity(usize::try_from(length)?);
                codec::encode_into(&mut bytes, value)?;
                if bytes.len() as u64 != length {
                    return Err(Error::contract("query encoded size changed"));
                }
                Ok(bytes)
            },
            equal: |left, right| {
                let left = left
                    .downcast_ref::<T>()
                    .ok_or_else(|| Error::invariant("query value type changed"))?;
                let right = right
                    .downcast_ref::<T>()
                    .ok_or_else(|| Error::invariant("query value type changed"))?;
                Ok(left == right)
            },
        }
    }
}

/// A loaded or newly computed Rust value held by the operation's working set.
///
/// The data is shared, so handing the answer to another caller does not copy it. Its optional
/// codec tells the evaluator how to compare old and new answers and tells the checkpoint how
/// to store one. Versions and dependencies live in the query record, separately from this object.
#[derive(Clone)]
pub(crate) struct Value {
    pub(crate) data: Data,
    pub(crate) codec: Option<Codec>,
}

impl Value {
    /// Return another [`Arc`] to the already loaded object with the caller's expected Rust type.
    /// This does not deserialize or copy the value. A mismatch means the runtime was asked to
    /// interpret a query result or output slot as a different type from the one it holds.
    pub(crate) fn typed<T: Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        self.data
            .clone()
            .downcast()
            .map_err(|_| Error::invariant("query value type changed"))
    }
}
