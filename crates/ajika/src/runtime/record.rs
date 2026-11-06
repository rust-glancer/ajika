//! The saved description of a query result and the reads that produced it.
//!
//! Reusing an answer after an edit or restart requires more than finding its bytes. Ajika
//! must know which inputs and child answers were used, and which versions were observed.
//! [`Record`] stores that evidence together with descriptions of the query's outputs. The types
//! here are the data exchanged between evaluation and storage; they do not perform either job.

use std::{collections::BTreeMap, sync::Arc};

use wincode::{SchemaRead, SchemaWrite};

use crate::{Durability, Lifetime, Persist, Result, Stamp, codec};

/// Fixed-size storage key derived from a query address or output slot. It identifies where
/// an answer belongs, so replacing the answer reuses the same storage key.
pub(crate) type Id = [u8; 32];

/// Identifies an answer that consumers may continue to treat as equal.
/// If a child recomputes to the same value, it keeps this token and its parents can reuse their
/// answers. A new token means equality with the previous answer could not be established.
/// This is separate from an input stamp and from the revision in which validation happened.
pub(crate) type Version = [u8; 16];

/// Selects one of the values produced by a query call. [`None`] is its ordinary return value;
/// [`Some`] is the encoded name/key of an additional value emitted with
/// [`db.output`](super::Db::output).
pub(crate) type Slot = Option<Vec<u8>>;

/// A query or input call represented as data: for example, the family `words` and key `"lib.rs"`.
///
/// Saved dependencies need to make the same call after restarting, when Rust function pointers
/// and in-process key tables are gone. The registry resolves the family name and decodes the key.
/// Keep this full address even when a hash is enough for storage lookup: if a child's saved
/// result is collected, its parent still needs the arguments with which to compute it again.
#[derive(Debug, Clone, PartialEq, Eq, Hash, SchemaRead, SchemaWrite)]
pub(crate) struct Address {
    /// Stable name of the query or input implementation, shared by all calls with different keys.
    pub(crate) family: String,
    pub(crate) key: Vec<u8>,
}

impl Address {
    pub(crate) fn new<K: Persist>(family: &str, key: &K) -> Result<Self> {
        Ok(Self {
            family: family.to_owned(),
            key: codec::encode(key)?,
        })
    }

    /// Find the storage key for this call's metadata and ordinary return value. Hash the call's
    /// identity, not its answer, so recomputation replaces the existing record rather than
    /// accumulating a separate historical entry for every result.
    pub(crate) fn id(&self) -> Id {
        let mut hash = blake3::Hasher::new();
        hash.update(&(self.family.len() as u64).to_le_bytes());
        hash.update(self.family.as_bytes());
        hash.update(&self.key);
        *hash.finalize().as_bytes()
    }

    /// Give each emitted value its own block address. All slots still belong to this query call,
    /// but storage can read or replace one payload without reading or replacing its siblings.
    pub(crate) fn value_id(&self, slot: &Slot) -> Id {
        let id = self.id();
        match slot {
            None => id,
            Some(slot) => {
                let mut hash = blake3::Hasher::new();
                hash.update(&id);
                hash.update(b"output");
                hash.update(slot);
                *hash.finalize().as_bytes()
            }
        }
    }
}

/// One direct read made by a query, including which answer it received.
///
/// An input read saves the application's content stamp. A child-query read saves that child's
/// output version. On reuse, Ajika makes the input or child current and compares this saved
/// observation with the new one. Equal observations allow the parent to keep its old answer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, SchemaRead, SchemaWrite)]
pub(crate) enum Dependency {
    /// The external observation this computation saw, such as a source file's content stamp.
    Input(Address, Stamp),
    /// The particular answer it used. Named outputs let siblings change independently.
    Value(Address, Slot, Version),
}

/// What validation and storage need to know about one output, without keeping its Rust value.
/// The version lets consumers check equality with the answer they used. The length tells disk
/// storage how many bytes to read; the query address and slot determine where those bytes live.
#[derive(Clone, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub(crate) struct ValueRecord {
    pub(crate) version: Version,
    /// Encoded payload length after a disk checkpoint. [`None`] also covers values without a
    /// disk payload: transient results, request-only results, and newly computed values.
    pub(crate) length: Option<u64>,
}

impl ValueRecord {
    /// Assign an identity to an answer whose equality with the old answer is not known.
    /// The payload has not been written yet; a disk checkpoint supplies its length if saved.
    pub(crate) fn fresh() -> Result<Self> {
        // Collection can remove the old record while a parent still remembers its version.
        // A fresh random token avoids accidentally reusing an old per-query counter value.
        let mut version = [0; 16];
        getrandom::fill(&mut version)?;
        Ok(Self {
            version,
            length: None,
        })
    }
}

/// What one completed query call read and produced, saved so the answer can be reused later.
///
/// For `word_count(file)`, this might record a read of `words(file)` at version W1 and a return
/// value at version C1. To reuse C1, the evaluator first checks whether `words(file)` still has
/// version W1. Neither value needs to be embedded here; payloads are stored separately.
///
/// A query may also emit named outputs. They share this one dependency list because one
/// execution produced them, but each output has its own version and stored payload. Recomputing
/// replaces the list of reads even when all answers compare equal, so future edits check the
/// dependencies of the execution that actually ran.
#[derive(Clone, SchemaRead, SchemaWrite)]
pub(crate) struct Record {
    // Keep this first in the encoding: known-valid reads decode only this output metadata.
    pub(crate) header: Arc<RecordHeader>,
    pub(crate) address: Address,
    // First-read order matters: a changed branch condition must stop validation before
    // the runtime tries to replay dependencies from the branch that is no longer selected.
    pub(crate) dependencies: Vec<Dependency>,
    /// Last recorded use in Unix milliseconds, used for age-based collection rather than validity.
    pub(crate) last_used: u64,
    /// Logical bytes charged to disk retention: this record's encoding plus its stored payloads.
    pub(crate) bytes: u64,
}

/// A query call's output metadata: slots, versions, stored payload lengths, and policies.
///
/// Once a memo knows the query is valid for the current inputs, a caller needs only this
/// metadata to select and load an answer. It is the first field of the stored [`Record`], so
/// loading it does not decode the dependency list. Callers share the loaded header for one
/// operation; a later input change may require loading the full record to check its reads.
#[derive(Clone, SchemaRead, SchemaWrite)]
pub(crate) struct RecordHeader {
    pub(crate) values: BTreeMap<Slot, ValueRecord>,
    /// Least durable input reached through this producer's reads.
    pub(crate) durability: Durability,
    /// Where this call's results may survive, inherited from its transitive inputs.
    /// This is independent of whether the return value has a codec: a transient value
    /// can still have persistent dependency metadata.
    pub(crate) lifetime: Lifetime,
}
