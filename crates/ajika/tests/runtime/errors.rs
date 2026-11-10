//! Error handling is observable through categories and typed payloads, independent of wrappers.

use std::sync::atomic::{AtomicBool, Ordering};

use ajika::{Error, ErrorKind, Result, ResultExt as _};

use super::{Db, inputs, retention};

#[derive(Debug, thiserror::Error)]
#[error("resource {name} is unavailable")]
struct ResourceUnavailable {
    name: &'static str,
}

#[ajika::query(transient)]
fn resource(_db: &Db) -> Result<()> {
    Err(Error::application(ResourceUnavailable { name: "catalog" }))
}

#[test]
fn application_payload_and_context_survive_parallel_sharing() -> anyhow::Result<()> {
    let mut database = ajika::Database::memory(inputs(""), (), retention());
    database.set_parallelism(2);
    let result = database.run(|db| {
        db.parallel_map(&[(), ()], |db, _| resource(db))
            .context("loading resources")
    });
    let error = result.expect_err("resource provider returned a failure");
    let ErrorKind::Application(source) = error.kind() else {
        panic!("context preserves application classification");
    };
    assert_eq!(
        source
            .downcast_ref::<ResourceUnavailable>()
            .expect("application payload")
            .name,
        "catalog"
    );
    assert_eq!(
        format!("{:#}", anyhow::Error::new(error)),
        "loading resources: application query failed: resource catalog is unavailable"
    );
    Ok(())
}

#[test]
fn application_accepts_boxed_errors_and_preserves_their_type() {
    let source: Box<dyn std::error::Error + Send + Sync> =
        Box::new(ResourceUnavailable { name: "catalog" });
    let result: std::result::Result<(), _> = Err(source);
    let error = result
        .map_err(Error::application)
        .expect_err("boxed resource error");
    let ErrorKind::Application(source) = error.kind() else {
        panic!("expected application error");
    };
    assert_eq!(
        source
            .downcast_ref::<ResourceUnavailable>()
            .expect("boxed error keeps its concrete type")
            .name,
        "catalog"
    );
}

#[test]
fn application_accepts_anyhow_reports_and_preserves_their_chain() {
    let result: anyhow::Result<()> =
        Err(anyhow::Error::new(ResourceUnavailable { name: "catalog" }).context("opening catalog"));
    let error = result
        .map_err(Error::application)
        .expect_err("resource report");
    assert!(matches!(error.kind(), ErrorKind::Application(_)));
    assert_eq!(
        format!("{:#}", anyhow::Error::new(error)),
        "application query failed: opening catalog: resource catalog is unavailable"
    );
}

#[test]
fn cancellation_keeps_its_checkpoint_and_the_next_operation_can_run() -> anyhow::Result<()> {
    let mut database = ajika::Database::memory(inputs(""), (), retention());
    let result = database
        .run(|_| -> Result<()> { Err(ajika::Cancelled::at("scan item").into()) })
        .context("analyzing a request");
    let error = result.expect_err("request stopped at a checkpoint");
    assert!(error.is_cancelled());
    let ErrorKind::Cancelled(cancelled) = error.kind() else {
        panic!("cancellation is an expected stop");
    };
    assert_eq!(cancelled.checkpoint(), "scan item");
    assert_eq!(*database.run(|db| super::leaf(db, 42))?, 42);
    Ok(())
}

#[derive(Default)]
struct HostCancellation(AtomicBool);

impl ajika::Services for HostCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[ajika::query]
fn answer(_db: &ajika::Db<'_, HostCancellation>) -> Result<u32> {
    Ok(42)
}

#[test]
fn host_cancellation_stops_runs_and_query_calls() -> Result<()> {
    let mut database =
        ajika::Database::memory(inputs(""), HostCancellation::default(), retention());
    assert_eq!(*database.run(answer)?, 42);

    database
        .run(|db| {
            db.services().0.store(true, Ordering::Relaxed);
            assert!(
                answer(db)
                    .expect_err("query observes host cancellation")
                    .is_cancelled()
            );
            Ok(())
        })
        .expect_err("operation remembers host cancellation");

    let result: Result<()> = database.run(|_| panic!("cancelled run must skip the callback"));
    assert!(result.expect_err("host is still cancelled").is_cancelled());

    database.set_services(HostCancellation::default());
    assert_eq!(*database.run(answer)?, 42);
    Ok(())
}

#[test]
fn an_observed_worker_failure_remains_a_failure_after_cancellation() -> anyhow::Result<()> {
    let mut database = ajika::Database::memory(inputs(""), (), retention());
    let cancellation = database.cancellation();
    let result: Result<()> = database.run(|db| {
        let original = resource(db).expect_err("resource provider failed");
        cancellation.cancel();
        let shared = db.check_cancelled().expect_err("operation has stopped");
        for error in [&original, &shared] {
            assert!(matches!(error.kind(), ErrorKind::Application(_)));
        }
        Err(shared)
    });
    assert!(matches!(
        result.expect_err("worker failure persists").kind(),
        ErrorKind::Application(_)
    ));
    Ok(())
}

#[test]
fn callback_cancellation_does_not_hide_a_recorded_query_failure() -> anyhow::Result<()> {
    let mut database = ajika::Database::memory(inputs(""), (), retention());
    let result: Result<()> = database.run(|db| {
        resource(db).expect_err("resource provider failed");
        Err(ajika::Cancelled::at("obsolete request").into())
    });
    let error = result.expect_err("query failure takes precedence over callback cancellation");
    assert!(matches!(error.kind(), ErrorKind::Application(_)));
    assert_eq!(*database.run(|db| super::leaf(db, 42))?, 42);
    Ok(())
}
