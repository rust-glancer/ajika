use std::any::{Any, type_name};

use crate::{Error, Interner, Result};

/// Facilities available while capturing an input. An initializer can resolve or intern immutable
/// IDs, borrow the snapshot's application state, and check whether acquisition should stop.
/// The captured value has no query dependencies of its own.
pub trait InputInit {
    /// Get a shared handle to the interner that owns the IDs in this input snapshot.
    fn interner(&self) -> Interner;

    /// Poll between chunks of acquisition and propagate the error to abandon the capture.
    /// Query reads and validation use the active operation's check. Host-side reads through
    /// a [`Database`](crate::Database) or [`Interner`] use the default check, which always succeeds.
    fn check_cancelled(&self) -> Result<()> {
        Ok(())
    }

    #[doc(hidden)]
    fn erased_state(&self) -> Option<&(dyn Any + Send + Sync)> {
        None
    }
}

impl dyn InputInit + '_ {
    /// Borrow the value supplied to [`Inputs::with_state`](super::Inputs::with_state).
    /// Returns a contract error if the snapshot has no state of the requested type.
    pub fn state<T: Send + Sync + 'static>(&self) -> Result<&T> {
        snapshot_state(self.erased_state())
    }
}

impl InputInit for Interner {
    fn interner(&self) -> Interner {
        self.clone()
    }
}

// A host can read a saved snapshot with just an interner, or with a database serving an
// editor overlay. The caller supplies the interner and cancellation for this read, while
// the snapshot that captures the key supplies the state.
pub(super) struct SnapshotInit<'a> {
    pub(super) caller: &'a dyn InputInit,
    pub(super) state: Option<&'a (dyn Any + Send + Sync)>,
}

impl InputInit for SnapshotInit<'_> {
    fn interner(&self) -> Interner {
        self.caller.interner()
    }

    fn check_cancelled(&self) -> Result<()> {
        self.caller.check_cancelled()
    }

    fn erased_state(&self) -> Option<&(dyn Any + Send + Sync)> {
        self.state
    }
}

pub(super) fn snapshot_state<T: Send + Sync + 'static>(
    state: Option<&(dyn Any + Send + Sync)>,
) -> Result<&T> {
    state
        .and_then(|state| state.downcast_ref::<T>())
        .ok_or_else(|| {
            Error::contract(format!(
                "input snapshot has no state of type `{}`",
                type_name::<T>()
            ))
        })
}
