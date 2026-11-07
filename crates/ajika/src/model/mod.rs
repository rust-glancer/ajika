//! The contracts an application supplies to make its computations reusable.
//!
//! [`Input`](crate::Input) describes a stored external value, such as a captured source file. [`Query`] describes answers
//! computed from that data and other queries. These traits tell the runtime how to identify or
//! compute a value; the runtime handles tracking dependencies and deciding when to do so.
//! The remaining types describe input identity, storage choices, reuse hints, and storage budgets.

use std::{hash::Hash, marker::PhantomData, sync::Arc, time::Duration};

use wincode::{SchemaRead, SchemaWrite};

use crate::{
    Result,
    codec::{self, Persist},
    runtime::{Db, value::Codec},
};

/// The application's way to tell Ajika whether an external input has changed.
///
/// For example, a source input can hash the captured file text. A query that reads the file
/// saves that stamp with its dependencies. On reuse, a matching stamp means the query would
/// observe the same text, even after restarting the process.
///
/// Cover everything the input value exposes to queries. Reuse the stamp when that value means
/// the same thing; a build counter alone would make unchanged inputs look different every time.
/// This identifies external data. Ajika separately assigns versions to computed query answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SchemaRead, SchemaWrite)]
pub struct Stamp([u8; 32]);

impl Stamp {
    /// Hash the actual captured bytes, for example the source text captured by an initializer.
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Hash a structured input, such as compiler options, using its serialized representation.
    /// Unordered containers should be normalized first when reproducible reuse matters.
    /// Serialization streams into the hash without allocating an encoded copy of the value.
    pub fn of_value<T: Persist>(value: &T) -> Result<Self> {
        let mut hasher = blake3::Hasher::new();
        codec::encode_into(
            wincode::io::std_write::WriteAdapter::new(&mut hasher),
            value,
        )?;
        Ok(Self::from_bytes(*hasher.finalize().as_bytes()))
    }

    /// Use an existing content fingerprint supplied by the host, without hashing it again.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// How long an input observation can justify reusing derived work.
///
/// Editor text is a session input: its results can serve later requests, but must not replace
/// saved results on disk. This is separate from durability, which describes how often it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, SchemaRead, SchemaWrite)]
pub enum Lifetime {
    /// Forget derived work when this operation ends.
    Operation,
    /// Keep derived work in the bounded session store, without writing it to disk.
    Session,
    /// Allow derived work to survive reopening the database.
    Persistent,
}

/// Work performed while serving an operation's queries, useful for finding why reuse is costly. A
/// query can avoid execution but still need dependency checks or value decoding, so these costs
/// have separate counters. They include work done for nested dependencies, not just the host's
/// top-level calls. [`stored_queries`](Statistics::stored_queries) and
/// [`stored_bytes`](Statistics::stored_bytes) describe storage after the operation.
///
/// Payload I/O counts exclude metadata and index overhead. For example, an equal
/// recomputation can write zero payload bytes while still replacing its saved dependency list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Statistics {
    /// Successful reuse or reads of stored results/outputs, rather than executions.
    pub hits: u64,
    /// Query executions completed and recorded in the operation, before its eventual commit.
    pub executed: u64,
    /// Stored query records whose dependencies were examined, excluding memoized decisions.
    pub validated: u64,
    /// Disk query-record reads for validation or computation, including misses, excluding maintenance.
    /// Known-valid queries need only their output header, counted separately below.
    pub metadata_reads: u64,
    /// Disk output-header reads, shared by all callers of a producer within an operation.
    pub header_reads: u64,
    /// Query records written by checkpointing, including metadata-only updates.
    pub metadata_writes: u64,
    /// Encoded query-record bytes written by checkpointing, excluding index overhead.
    pub metadata_bytes_written: u64,
    /// Values decoded from storage, including old values loaded for equality comparisons.
    pub decoded: u64,
    /// Encoded payload bytes loaded for decoding, including equality comparisons.
    pub bytes_read: u64,
    /// Payload bytes encoded during checkpointing, even if the eventual commit fails.
    pub bytes_written: u64,
    /// Previous values decoded for equality, included in [`decoded`](Statistics::decoded) and
    /// [`bytes_read`](Statistics::bytes_read).
    pub compared: u64,
    /// Recomputed values that kept their previous result version because the answer stayed equal.
    pub unchanged: u64,
    /// Calls served directly from the operation's decoded working set.
    pub resident_hits: u64,
    /// Times a worker waited for another worker's query claim.
    pub waits: u64,
    /// Complete query records reclaimed at an operation boundary.
    pub collected: u64,
    /// Complete cache records after the operation's optional collection.
    pub stored_queries: u64,
    /// Logical encoded bytes, excluding index overhead and allocator capacity.
    /// The typed memory backend does not serialize values to produce this counter.
    pub stored_bytes: u64,
    /// Session records retained after the operation, separate from the durable cache.
    pub session_queries: u64,
    /// Encoded-size estimate of retained session records and changed payloads. Disk-backed
    /// equal outputs are excluded. This is a budget measure, not allocator accounting.
    pub session_bytes: u64,
}

/// A host-supplied classification that lets Ajika skip checks after unrelated input changes.
///
/// For example, classify source text as [`Low`](Durability::Low) and compiler configuration as
/// [`High`](Durability::High). A source edit can then leave configuration-only queries valid
/// without checking their inputs. A query that reads both inherits [`Low`](Durability::Low); its
/// least durable dependency decides.
///
/// Each field declares its class. Publishing an input batch considers both the old and new
/// classes of changed fields, so moving a saved file into an editor view also invalidates
/// proofs made under its previous, more durable policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, SchemaRead, SchemaWrite)]
pub enum Durability {
    Low,
    Medium,
    High,
}

/// Limits how much previously computed work the backend keeps for future requests.
///
/// Collection removes the least recently used query results when these budgets are exceeded.
/// Removed results can be computed again; this controls storage growth, not query validity.
/// The operation releases its working references separately; these limits govern what the
/// backend continues to keep. The host chooses budgets appropriate to its workspace.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    /// Encoded byte budget for retained records and payloads. Session results use an estimated
    /// encoded size. Memory databases apply this limit only to session results.
    pub max_bytes: u64,
    /// Number of stored query calls. A query's return value and named outputs count as one entry.
    pub max_entries: usize,
    /// Time since a producer was last used before it becomes eligible for collection.
    /// This is a storage budget, not a validity check: input changes are checked separately.
    pub max_idle: Duration,
}

/// A computation whose answer Ajika can remember and reuse when its dependencies stay equal.
///
/// For example, `Words` might take a file path, read a `FileText` input, and return its words.
/// Another query can call `Words` through [`db.query`](Db::query); Ajika decides whether to use the
/// previous answer or run [`execute`](Query::execute). Reads made through [`Db`] automatically
/// become dependencies, so the query implementation only describes how to compute the answer.
///
/// Prefer [`#[ajika::query]`](macro@crate::query), which generates this trait implementation around
/// an ordinary function. Handwritten implementations use
/// [`#[ajika::register]`](macro@crate::register) for replay after restarting. In both forms,
/// external data affecting the answer must be read through inputs or queries: an untracked
/// filesystem read would be invisible when Ajika decides to reuse an old answer.
pub trait Query: 'static {
    /// Untracked host facilities, such as progress reporting. Semantic values live in [`Inputs`](crate::Inputs).
    type Services: Send + Sync + 'static;

    /// Arguments identifying the same logical question across edits, such as a file path.
    /// Avoid including the whole input revision: that creates a different call after every
    /// edit, preventing Ajika from finding the previous answer to validate and compare.
    type Key: Persist + Clone + Eq + Hash + Send + Sync + 'static;

    type Value: Send + Sync + 'static;

    /// Stable replay name, unique among query implementations for this database's
    /// [`Services`](Query::Services) type. Together with the key, this identifies the same computation
    /// across revisions and restarts.
    const NAME: &'static str;

    /// Compute an answer from the key and tracked reads through `db`. Ajika calls this when
    /// there is no reusable answer, or when a transient value must be reconstructed.
    /// Given equal observations, this must produce an equal answer. Do not rely on side effects:
    /// a stored or resident answer skips execution entirely.
    fn execute(db: &Db<'_, Self::Services>, key: Self::Key) -> Result<Arc<Self::Value>>;
    /// Choose whether the return value can be kept by the backend between operations.
    /// This controls the payload; Ajika can still remember dependencies for a transient value.
    fn storage() -> Storage<Self::Value>;

    /// Check that a newly computed or decoded value is usable, for example that indices in
    /// its arenas are in bounds. This is an optional check of the value's own structure, not
    /// dependency validation. It does not run on every resident hit or collect tracked reads.
    fn validate(_value: &Self::Value) -> Result<()> {
        Ok(())
    }
}

/// A query's choice about keeping its return value after the operation finishes.
///
/// [`persistent`](Storage::persistent) allows the backend to save the value for reuse.
/// [`transient`](Storage::transient) keeps the value only while this operation needs it. This is useful
/// for intermediates such as syntax trees that should not need serialization. Both choices still
/// allow Ajika to remember the query's reads. Callers use the same query function either way; the
/// declaration makes this choice once.
///
/// This policy is separate from the database backend. [`Database::memory`](crate::Database::memory)
/// keeps persistent-policy values as typed objects instead of disk bytes. It still releases
/// transient query values at operation end. Request-only inputs can prevent retention regardless
/// of this payload policy.
pub struct Storage<T> {
    pub(crate) codec: Option<Codec>,
    marker: PhantomData<fn() -> T>,
}

impl<T: Send + Sync + 'static> Storage<T> {
    /// Share the return value within the operation, then release Ajika's references to it.
    /// Asking for the value in a later operation runs the query again. Checking a saved parent's
    /// dependencies can still prove this query unchanged without reconstructing that value.
    ///
    /// No serialization or equality implementation is required. If dependencies changed,
    /// Ajika must conservatively assign a new version; it has no value comparison to prove equality.
    pub fn transient() -> Self {
        Self {
            codec: None,
            marker: PhantomData,
        }
    }
}

impl<T: Persist + PartialEq + Send + Sync + 'static> Storage<T> {
    /// Keep the value in the selected backend so later operations can load it without execution.
    /// [`PartialEq`] decides whether recomputation changed the answer. It must cover everything
    /// callers can observe: when values compare equal, Ajika may keep the old stored value.
    pub fn persistent() -> Self {
        Self {
            codec: Some(Codec::of::<T>()),
            marker: PhantomData,
        }
    }
}
