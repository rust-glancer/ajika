//! Handles for separately readable values produced by one query computation.
//!
//! Some work naturally produces several answers together, but consumers do not need all of
//! them. A query can emit named outputs and return a set of [`Output`] handles.
//! This module defines those handles, how they identify a value, and how callers ask to read it.
//! Execution and dependency tracking for those reads belong to the evaluator.

use std::{
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::MaybeUninit,
    sync::Arc,
};

use wincode::{SchemaRead, SchemaWrite};

use super::{Db, record::Address};
use crate::{Persist, Result, codec};

/// Names one additional value within the query that emits it. A fixed name such as `"declarations"`
/// identifies a single output; `("body", id)` identifies one of many outputs of the same kind.
/// [`db.output`](Db::output) accepts both forms through this trait. Recomputations must use the
/// same name/key for the same logical output; emission order is irrelevant.
#[doc(hidden)]
pub trait OutputSlot {
    fn encode(self) -> Result<Vec<u8>>;
}

impl OutputSlot for &str {
    fn encode(self) -> Result<Vec<u8>> {
        codec::encode(&(self.to_owned(), None::<Vec<u8>>))
    }
}

impl OutputSlot for String {
    fn encode(self) -> Result<Vec<u8>> {
        codec::encode(&(self, None::<Vec<u8>>))
    }
}

impl<K: Persist> OutputSlot for (&str, K) {
    fn encode(self) -> Result<Vec<u8>> {
        codec::encode(&(self.0.to_owned(), Some(codec::encode(&self.1)?)))
    }
}

/// The query call and slot needed to find an output again. There is no result version here:
/// the same identity asks for that slot under whichever input revision is being queried.
#[derive(Debug, Clone, PartialEq, Eq, Hash, SchemaRead, SchemaWrite)]
struct Identity {
    producer: Address,
    slot: Vec<u8>,
}

/// A handle for one additional value emitted by a query with [`db.output`](Db::output).
///
/// For example, a declaration-building query can emit separate shape and location tables, then
/// return their handles. A consumer holding the shape handle can call [`value(db)`](Output::value)
/// to read just that table. It depends on the shape's answer, so changing only locations need not
/// rerun the consumer. Both tables are still computed together when their producer needs execution.
///
/// The handle holds an address, not the table or a particular version of it. Keeping or saving
/// the handle does not keep the table in memory. Two handles for the same query/key/slot compare
/// equal even when its data changes, allowing a set of output handles to remain equal. Reading
/// through [`value`](Output::value) or [`value_ref`](Output::value_ref) establishes the separate
/// dependency on the data actually used.
pub struct Output<T> {
    identity: Identity,
    marker: PhantomData<fn() -> T>,
}

impl<T> Output<T> {
    pub(crate) fn new(producer: Address, slot: Vec<u8>) -> Self {
        Self {
            identity: Identity { producer, slot },
            marker: PhantomData,
        }
    }
}

impl<T: Persist + PartialEq + Send + Sync + 'static> Output<T> {
    /// Ask for this output under the operation's captured inputs, loading or computing as needed.
    /// A handle kept from an earlier revision asks for the new answer, not the old one.
    ///
    /// Inside a query, the read records this output's version so the caller can be reused while
    /// this particular answer stays equal. It does not depend on sibling outputs. If the producer
    /// no longer emits this slot, the handle cannot supply a value and the read returns an error.
    pub fn value<S: Send + Sync + 'static>(&self, db: &Db<'_, S>) -> Result<Arc<T>> {
        db.output_value(&self.identity.producer, &self.identity.slot)
    }

    /// Read this output with the same dependency tracking as [`value`](Self::value).
    /// The reference lasts for the lifetime of the supplied [`Db`], even if the handle
    /// is dropped first. Use `value` to keep the answer after the operation ends.
    pub fn value_ref<'db, S: Send + Sync + 'static>(&self, db: &Db<'db, S>) -> Result<&'db T> {
        let value = db.output_value(&self.identity.producer, &self.identity.slot)?;
        // SAFETY: output_value returns the Arc kept in db's frame. The frame never
        // removes or replaces acquired outputs and is borrowed for 'db.
        Ok(unsafe { &*Arc::as_ptr(&value) })
    }
}

impl<T> Clone for Output<T> {
    fn clone(&self) -> Self {
        Self {
            identity: self.identity.clone(),
            marker: PhantomData,
        }
    }
}

impl<T> PartialEq for Output<T> {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}

impl<T> Eq for Output<T> {}

impl<T> Hash for Output<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity.hash(state);
    }
}

impl<T> fmt::Debug for Output<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.identity.fmt(formatter)
    }
}

// Only the identity is encoded. T names the value to decode later and need not have
// any particular representation for the selector itself to round-trip.
unsafe impl<T, C: wincode::config::Config> SchemaWrite<C> for Output<T> {
    type Src = Self;

    const TYPE_META: wincode::TypeMeta = wincode::TypeMeta::Dynamic;

    fn size_of(src: &Self) -> wincode::WriteResult<usize> {
        <Identity as SchemaWrite<C>>::size_of(&src.identity)
    }

    fn write(writer: impl wincode::io::Writer, src: &Self) -> wincode::WriteResult<()> {
        <Identity as SchemaWrite<C>>::write(writer, &src.identity)
    }
}

// Decode the address as ordinary owned data, then construct a typed selector around it.
// No T is decoded here; T is only needed when value() later asks storage for the payload.
unsafe impl<'de, T, C: wincode::config::Config> SchemaRead<'de, C> for Output<T> {
    type Dst = Self;
    const TYPE_META: wincode::TypeMeta = wincode::TypeMeta::Dynamic;

    fn read(
        reader: impl wincode::io::Reader<'de>,
        dst: &mut MaybeUninit<Self>,
    ) -> wincode::ReadResult<()> {
        let identity = <Identity as SchemaRead<'de, C>>::get(reader)?;
        dst.write(Self {
            identity,
            marker: PhantomData,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use super::{Address, Output, OutputSlot};
    use crate::{Database, Inputs, Result, Retention};

    #[test]
    fn borrowed_output_survives_a_second_publication() -> Result<()> {
        let mut database = Database::memory(
            Inputs::default(),
            (),
            Retention {
                max_entries: 1,
                max_bytes: 1024,
                max_idle: Duration::from_secs(60),
            },
        );
        database.run(|db| {
            let producer = Address::new("producer", &())?;
            let slot = "text".encode()?;

            // The frame is the sole owner, as it can be after a producer rebuilds its memo.
            db.frame
                .remember(&producer, &slot, Arc::new(String::from("hello")))?;
            let borrowed = Output::<String>::new(producer.clone(), slot.clone()).value_ref(db)?;

            // Simulate a second reader publishing a different allocation for the same slot
            // after losing the race to fill the frame cache.
            let published = db
                .frame
                .remember(&producer, &slot, Arc::new(String::from("hello")))?;
            assert_eq!(borrowed, "hello");
            assert!(std::ptr::eq(borrowed, published.as_ref()));
            Ok(())
        })
    }
}
