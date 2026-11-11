//! Explain evaluation decisions without doing any evaluation of our own.
//!
//! The evaluator supplies outcomes it has already established. These helpers only render
//! identities and emit events; they never fetch records or values. Keep calls outside runtime
//! locks, since a tracing subscriber is application code and can do arbitrary work.

use std::fmt;

use super::{
    Operation,
    evaluate::{OutputOutcome, ReuseReason},
    record::{Address, Dependency, Slot},
};
use crate::{Services, codec};

pub(super) const TARGET: &str = "ajika::diagnostics";

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

impl<S: Services> Operation<'_, S> {
    pub(super) fn diagnostic_execute(&self, address: &Address, reason: &'static str) {
        if tracing::enabled!(target: TARGET, tracing::Level::DEBUG) {
            let id = address.id();
            tracing::debug!(target: TARGET,
                op = self.id, revision = self.database.revision,
                query = address.family, call = %Hex(&id.0[..4]), call_id = %Hex(&id.0),
                reason, "execute");
        }
    }

    pub(super) fn diagnostic_reuse(&self, address: &Address, slot: &Slot, reason: ReuseReason) {
        if tracing::enabled!(target: TARGET, tracing::Level::TRACE) {
            let id = address.id();
            let output = SlotLabel::new(slot);
            let reason = match reason {
                ReuseReason::ResidentValue => "resident_value",
                ReuseReason::ValidityProof => "validity_proof",
                ReuseReason::Durability => "durability",
                ReuseReason::DependenciesValid => "dependencies_valid",
            };
            tracing::trace!(target: TARGET,
                op = self.id, revision = self.database.revision,
                query = address.family, call = %Hex(&id.0[..4]), call_id = %Hex(&id.0),
                output = output.0, output_id = %Hex(&address.value_id(slot).0),
                reason, "reuse");
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
                query = parent.family, call = %Hex(&id.0[..4]), call_id = %Hex(&id.0),
                dependency = address.family, dependency_call = %Hex(&child.0[..4]),
                dependency_id = %Hex(&child.0), dependency_kind = kind,
                output = output.0, output_id = %Hex(&address.value_id(slot).0),
                reason, "invalidated");
        }
    }

    pub(super) fn diagnostic_output(&self, address: &Address, slot: &Slot, outcome: OutputOutcome) {
        if tracing::enabled!(target: TARGET, tracing::Level::TRACE) {
            let id = address.id();
            let output = SlotLabel::new(slot);
            let (result, evidence, version) = match outcome {
                OutputOutcome::Added => ("added", "none", "created"),
                OutputOutcome::EqualByComparison => ("equal", "comparison", "preserved"),
                OutputOutcome::EqualByProof => ("equal", "validity_proof", "preserved"),
                OutputOutcome::Unequal => ("unequal", "comparison", "advanced"),
                OutputOutcome::ComparisonUnavailable => {
                    ("comparison_unavailable", "none", "advanced")
                }
                OutputOutcome::Removed => ("removed", "none", "removed"),
            };
            tracing::trace!(target: TARGET,
                op = self.id, revision = self.database.revision,
                query = address.family, call = %Hex(&id.0[..4]), call_id = %Hex(&id.0),
                output = output.0, output_id = %Hex(&address.value_id(slot).0),
                result, evidence, version, "output");
        }
    }
}
