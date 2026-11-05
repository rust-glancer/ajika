//! Runtime support that generated declarations need from an application crate.
//!
//! The macros use these items to register queries and inputs, recover signature types, and
//! connect interned IDs to the interner. The wincode re-export also lets output declarations
//! derive serialization without requiring a direct dependency in the declaring crate. These items
//! are public so expanded code can name them across crate boundaries; application code uses the
//! generated APIs instead.

use std::{any::Any, sync::Arc};

pub use inventory;
pub use wincode;

pub use crate::interner::Interned;
use crate::{Db, Input, Query, Result, runtime::registry::Registry};

/// A linked declaration's instruction for adding itself to a database's replay registry.
///
/// Macro expansions submit these to inventory, so the program discovers declarations across
/// crates without a manually maintained list. The installer takes an erased registry because
/// declarations can use different host-service types. It only installs into a matching one.
pub struct Registration {
    pub install: fn(&mut dyn Any),
}
inventory::collect!(Registration);

/// Install Q when this database uses Q's host-service type. Other databases see the same
/// linked registration list, so a different service type is skipped rather than treated as an error.
pub fn install_query<Q: Query>(registry: &mut dyn Any) {
    if let Some(registry) = registry.downcast_mut::<Registry<Q::Services>>() {
        registry.query::<Q>();
    }
}

/// Install a typed input field independently of the database's host services.
pub fn install_input<T: Input>(registry: &mut dyn Any) {
    if let Some(registry) = registry.downcast_mut::<crate::input::registry::Registry>() {
        registry.input::<T>();
    }
}

/// Extract the host-service type from a query function's database parameter.
/// For a user alias `type AppDb<'a> = ajika::Db<'a, Services>`, the macro can ask for
/// `<fn(&AppDb) as DbType>::Services` and let Rust resolve it to Services. Parsing the alias's
/// spelling in the macro would not be enough to discover that type.
pub trait DbType {
    type Services: Send + Sync + 'static;
}
impl<S: Send + Sync + 'static> DbType for fn(&Db<'_, S>) {
    type Services = S;
}

/// Extract the query payload from its declared result, including through type aliases.
/// Ordinary bodies return `Result<T>`; `forward` bodies return `Result<Arc<T>>` so the runtime
/// can reuse their allocation. The mode keeps an ordinary Arc-valued payload distinct from
/// forwarding the shared pointer itself.
pub trait ResultType<const FORWARD: bool> {
    type Value: Send + Sync + 'static;
}
impl<T: Send + Sync + 'static> ResultType<false> for Result<T> {
    type Value = T;
}
impl<T: Send + Sync + 'static> ResultType<true> for Result<Arc<T>> {
    type Value = T;
}
