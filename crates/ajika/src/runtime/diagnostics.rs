//! Explain evaluation decisions without doing any evaluation of our own.
//!
//! The evaluator supplies outcomes it has already established. These helpers only render
//! identities and emit events; they never fetch records or values. Keep calls outside runtime
//! locks, since a tracing subscriber is application code and can do arbitrary work.

use std::fmt;

use super::{
    Operation,
    record::{Address, Dependency, Slot},
};
use crate::codec;

pub(super) const TARGET: &str = "ajika::diagnostics";

/// The existing evidence that lets evaluation skip a provider. Remember which proof succeeded
/// while loading a value, so the resulting event explains reuse without rechecking anything.
#[derive(Clone, Copy)]
pub(super) enum ReuseReason {
    ResidentValue,
    ValidityProof,
    Durability,
    DependenciesValid,
}

impl ReuseReason {
    fn name(self) -> &'static str {
        match self {
            Self::ResidentValue => "resident_value",
            Self::ValidityProof => "validity_proof",
            Self::Durability => "durability",
            Self::DependenciesValid => "dependencies_valid",
        }
    }
}

/// What we know about one new answer compared with its predecessor. A fresh version does not
/// necessarily mean the values differ: a transient predecessor may be unavailable to compare.
#[derive(Clone, Copy)]
pub(super) enum OutputOutcome {
    Added,
    EqualByComparison,
    EqualByProof,
    Unequal,
    ComparisonUnavailable,
    Removed,
}

impl OutputOutcome {
    pub(super) fn preserves_version(self) -> bool {
        matches!(self, Self::EqualByComparison | Self::EqualByProof)
    }

    fn fields(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Added => ("added", "none", "created"),
            Self::EqualByComparison => ("equal", "comparison", "preserved"),
            Self::EqualByProof => ("equal", "validity_proof", "preserved"),
            Self::Unequal => ("unequal", "comparison", "advanced"),
            Self::ComparisonUnavailable => ("comparison_unavailable", "none", "advanced"),
            Self::Removed => ("removed", "none", "removed"),
        }
    }
}

/// Hex formatting borrows existing IDs; shortening is only a presentation choice.
pub(super) struct Hex<'a>(pub(super) &'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Decode only the slot address, never its payload. Keys stay opaque even if their Rust types
/// happen to implement Debug. Custom OutputSlot encodings can still be identified by output_id.
struct SlotLabel(String);

impl SlotLabel {
    fn new(slot: &Slot) -> Self {
        let Some(bytes) = slot else {
            return Self("<return>".to_owned());
        };
        let Ok((name, key)) = codec::decode::<(String, Option<Vec<u8>>)>(bytes) else {
            return Self("<opaque>".to_owned());
        };
        Self(match key {
            Some(key) => format!("{name}#{}", Hex(&blake3::hash(&key).as_bytes()[..4])),
            None => name,
        })
    }
}

impl<S: Send + Sync + 'static> Operation<'_, S> {
    pub(super) fn diagnostic_execute(&self, address: &Address, reason: &'static str) {
        if tracing::enabled!(target: TARGET, tracing::Level::DEBUG) {
            let id = address.id();
            tracing::debug!(target: TARGET,
                op = self.id, revision = self.database.revision,
                query = address.family, call = %Hex(&id[..4]), call_id = %Hex(&id),
                reason, "execute");
        }
    }

    pub(super) fn diagnostic_reuse(&self, address: &Address, slot: &Slot, reason: ReuseReason) {
        if tracing::enabled!(target: TARGET, tracing::Level::TRACE) {
            let id = address.id();
            let output = SlotLabel::new(slot);
            tracing::trace!(target: TARGET,
                op = self.id, revision = self.database.revision,
                query = address.family, call = %Hex(&id[..4]), call_id = %Hex(&id),
                output = output.0, output_id = %Hex(&address.value_id(slot)),
                reason = reason.name(), "reuse");
        }
    }

    pub(super) fn diagnostic_invalidated(
        &self,
        parent: &Address,
        dependency: &Dependency,
        reason: &'static str,
    ) {
        if tracing::enabled!(target: TARGET, tracing::Level::DEBUG) {
            let (address, slot, kind) = match dependency {
                Dependency::Input(address, _) => (address, &None, "input"),
                Dependency::Value(address, slot, _) => (address, slot, "query"),
            };
            let id = parent.id();
            let child = address.id();
            let output = SlotLabel::new(slot);
            tracing::debug!(target: TARGET,
                op = self.id, revision = self.database.revision,
                query = parent.family, call = %Hex(&id[..4]), call_id = %Hex(&id),
                dependency = address.family, dependency_call = %Hex(&child[..4]),
                dependency_id = %Hex(&child), dependency_kind = kind,
                output = output.0, output_id = %Hex(&address.value_id(slot)),
                reason, "invalidated");
        }
    }

    pub(super) fn diagnostic_output(&self, address: &Address, slot: &Slot, outcome: OutputOutcome) {
        if tracing::enabled!(target: TARGET, tracing::Level::TRACE) {
            let id = address.id();
            let output = SlotLabel::new(slot);
            let (result, evidence, version) = outcome.fields();
            tracing::trace!(target: TARGET,
                op = self.id, revision = self.database.revision,
                query = address.family, call = %Hex(&id[..4]), call_id = %Hex(&id),
                output = output.0, output_id = %Hex(&address.value_id(slot)),
                result, evidence, version, "output");
        }
    }
}
