use std::{
    panic,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use ajika::{Cancelled, Database, Inputs, QueryCycle, Result, Retention};
use anyhow::Context as _;

use super::{Sources, inputs, leaf, retention, symbols, tree};

struct Control {
    enabled: AtomicBool,
    entered: Barrier,
}
type Db<'a> = ajika::Db<'a, Arc<Control>>;
#[ajika::input(key = (), lifetime = Operation)]
struct OperationControl {
    enabled: bool,
}

fn control_inputs(control: &Control) -> Inputs {
    Inputs::default()
        .edit(|update| {
            OperationControl::at(()).set_enabled(update, control.enabled.load(Ordering::Acquire))
        })
        .expect("boolean input")
}

#[ajika::query]
fn wait_for_cancellation(db: &Db) -> Result<u32> {
    if OperationControl::at(()).enabled(db)? {
        db.services().entered.wait();
        loop {
            db.check_cancelled()?;
            thread::yield_now();
        }
    }
    Ok(42)
}

#[test]
fn cancelling_parallel_demands_drains_them_and_the_next_operation_can_retry() -> Result<()> {
    let control = Arc::new(Control {
        enabled: AtomicBool::new(true),
        entered: Barrier::new(2),
    });
    let directory = tempfile::tempdir()?;
    let mut db = Database::open(
        directory.path().join("queries"),
        "test",
        control_inputs(&control),
        control.clone(),
        retention(),
    )?;
    db.set_parallelism(2);
    let cancellation = db.cancellation();
    let result = thread::scope(|scope| {
        scope.spawn(|| {
            control.entered.wait();
            cancellation.cancel();
        });
        db.run(|db| db.parallel_map(&[(), ()], |db, _| wait_for_cancellation(db)))
    });
    assert!(
        result
            .expect_err("operation was cancelled")
            .is::<Cancelled>()
    );
    control.enabled.store(false, Ordering::Release);
    db.replace_inputs(control_inputs(&control));
    assert_eq!(*db.run(wait_for_cancellation)?, 42);
    cancellation.cancel();
    assert_eq!(
        *db.run(wait_for_cancellation)?,
        42,
        "an old token cannot cancel a new operation"
    );
    assert_eq!(
        db.statistics().executed,
        1,
        "request-only records leave with their operation"
    );
    assert_eq!(db.statistics().stored_queries, 0);
    Ok(())
}

#[ajika::query]
fn may_panic(db: &Db) -> Result<u32> {
    if OperationControl::at(()).enabled(db)? {
        panic!("test provider unwound");
    }
    Ok(42)
}

#[test]
fn an_unwinding_provider_leaves_no_claim_or_uncommitted_value() -> Result<()> {
    let control = Arc::new(Control {
        enabled: AtomicBool::new(true),
        entered: Barrier::new(1),
    });
    let mut db = Database::memory(control_inputs(&control), control.clone(), retention());
    let panic = panic::catch_unwind(panic::AssertUnwindSafe(|| db.run(may_panic)));
    assert!(panic.is_err());
    control.enabled.store(false, Ordering::Release);
    db.replace_inputs(control_inputs(&control));
    assert_eq!(*db.run(may_panic)?, 42);
    assert_eq!(db.statistics().executed, 1);
    Ok(())
}

#[ajika::query(forward)]
fn mutually_recursive(db: &Db, key: u32) -> Result<Arc<u32>> {
    if OperationControl::at(()).enabled(db)? {
        db.services().entered.wait();
        return recursive_peer(db, 1 - key).context("building declarations");
    }
    Ok(Arc::new(key))
}

#[ajika::query(forward)]
fn recursive_peer(db: &Db, key: u32) -> Result<Arc<u32>> {
    mutually_recursive(db, key).context("resolving associated items")
}

fn cycle_in(error: &anyhow::Error) -> &QueryCycle {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<QueryCycle>())
        .expect("structured cycle survives worker failures")
}

#[test]
fn a_cross_worker_cycle_finishes_and_does_not_poison_the_next_operation() -> Result<()> {
    let control = Arc::new(Control {
        enabled: AtomicBool::new(true),
        entered: Barrier::new(2),
    });
    let mut db = Database::memory(control_inputs(&control), control.clone(), retention());
    db.set_parallelism(2);
    let result: Result<()> = db.run(|db| {
        let error = db
            .parallel_map(&[0, 1], |db, key| mutually_recursive(db, *key))
            .expect_err("queries form a cycle");
        let shared = db
            .check_cancelled()
            .expect_err("operation remembers the failure");
        assert_eq!(cycle_in(&error).calls, cycle_in(&shared).calls);
        Err(error)
    });
    let error = result.expect_err("queries form a cycle");
    let cycle = cycle_in(&error);
    assert_eq!(error.to_string(), "building declarations");
    let report = format!("{error:#}");
    assert!(report.contains("resolving associated items"));
    assert!(report.contains("query cycle:"));
    assert_eq!(cycle.calls.len(), 5);
    assert_eq!(cycle.calls.first(), cycle.calls.last(), "closed call chain");
    assert_ne!(
        cycle.calls[0].id, cycle.calls[1].id,
        "different participating calls"
    );
    assert!(
        cycle
            .calls
            .iter()
            .all(|call| call.query.ends_with("::mutually_recursive")
                || call.query.ends_with("::recursive_peer"))
    );
    assert!(
        cycle
            .calls
            .windows(2)
            .all(|pair| pair[0].query != pair[1].query),
        "witness follows alternating calls"
    );
    let display = cycle.to_string();
    assert!(display.starts_with("query cycle:\n"));
    assert!(display.contains(&format!("→ {}", cycle.calls[1])));
    control.enabled.store(false, Ordering::Release);
    db.replace_inputs(control_inputs(&control));
    assert_eq!(*db.run(|db| mutually_recursive(db, 0))?, 0);
    Ok(())
}

#[test]
fn a_panicking_parallel_callback_wakes_other_callbacks_before_joining() -> Result<()> {
    let mut db = Database::memory(ajika::Inputs::default(), (), retention());
    db.set_parallelism(2);
    let entered = Barrier::new(2);
    let panic = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        db.run(|db| {
            db.parallel_map(&[0, 1], |db, key| -> Result<()> {
                entered.wait();
                if *key == 0 {
                    panic!("callback failed outside a query");
                }
                loop {
                    db.check_cancelled()?;
                    thread::yield_now();
                }
            })
        })
    }));
    assert!(panic.is_err());
    assert_eq!(db.run(|_| Ok(42))?, 42);
    Ok(())
}

#[test]
fn a_failed_operation_discards_new_work_but_keeps_the_previous_checkpoint() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(&path, "test", inputs("old"), (), retention())?;
    db.run(|db| symbols(db, 1))?;
    db.replace_inputs(inputs("new"));
    let failed: Result<()> = db.run(|db| {
        symbols(db, 1)?;
        anyhow::bail!("discard this operation")
    });
    assert!(failed.is_err());
    drop(db);

    let mut db = Database::open(&path, "test", inputs("old"), (), retention())?;
    assert_eq!(db.run(|db| symbols(db, 1))?.as_str(), "symbols:1:old");
    assert_eq!(db.statistics().executed, 0);
    db.replace_inputs(inputs("new"));
    assert_eq!(db.run(|db| symbols(db, 1))?.as_str(), "symbols:1:new");
    Ok(())
}

#[test]
fn cold_record_collection_bounds_keys_and_collected_dependencies_replay() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let limits = Retention {
        max_entries: 1,
        ..retention()
    };
    let mut db = Database::open(&path, "test", inputs("hello"), (), limits)?;
    for file in 0..12 {
        assert_eq!(
            db.run(|db| symbols(db, file))?.as_str(),
            format!("symbols:{file}:hello")
        );
        assert!(db.statistics().stored_queries <= 1);
    }
    drop(db);
    let mut db = Database::open(&path, "test", inputs("hello"), (), limits)?;
    assert_eq!(db.run(|db| symbols(db, 0))?.as_str(), "symbols:0:hello");
    assert!(db.statistics().stored_queries <= 1);
    assert!(db.statistics().collected > 0);
    Ok(())
}

#[test]
fn a_new_schema_epoch_and_explicit_invalidation_start_fresh() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(&path, "one", Sources::default().capture(), (), retention())?;
    db.run(|db| leaf(db, 1))?;
    drop(db);
    let mut db = Database::open(&path, "two", Sources::default().capture(), (), retention())?;
    db.run(|db| leaf(db, 1))?;
    assert_eq!(db.statistics().executed, 1);
    db.invalidate()?;
    db.run(|db| leaf(db, 1))?;
    assert_eq!(db.statistics().executed, 1);
    Ok(())
}

#[test]
fn replacing_large_payloads_reclaims_their_logical_storage() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let large = "x".repeat(200_000);
    let mut db = Database::open(&path, "test", inputs(&large), (), retention())?;
    db.run(|db| tree(db, 1))?;
    let old_bytes = db.statistics().stored_bytes;
    drop(db);

    // Reopening must recover a large value from its independently stored payload.
    let mut db = Database::open(&path, "test", inputs(&large), (), retention())?;
    assert_eq!(db.run(|db| tree(db, 1))?.as_str(), format!("1:{large}"));
    assert_eq!(db.statistics().executed, 0);
    db.replace_inputs(inputs("small"));
    db.run(|db| tree(db, 1))?;
    assert!(db.statistics().stored_bytes < old_bytes / 10);
    drop(db);
    let mut db = Database::open(&path, "test", inputs("small"), (), retention())?;
    assert_eq!(db.run(|db| tree(db, 1))?.as_str(), "1:small");
    assert_eq!(db.statistics().executed, 0);
    Ok(())
}

#[test]
fn retention_also_reclaims_obsolete_payloads_between_operations() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let text = "x".repeat(256 * 1024);
    let limits = Retention {
        max_entries: 8,
        ..retention()
    };
    let mut db = Database::open(&path, "test", inputs(&text), (), limits)?;
    db.set_compression(ajika::Compression::None);
    // These results share a pack. Retention removes most of them after the operation;
    // automatic maintenance must reclaim their bytes while keeping the remaining answers.
    db.run(|db| {
        for file in 0..32 {
            tree(db, file)?;
        }
        Ok(())
    })?;
    let storage = db.storage_statistics()?.expect("disk storage");
    assert_eq!(storage.values, 8);
    assert_eq!(storage.dead_pack_bytes, 0);
    assert!(storage.pack_file_bytes < 3 * 1024 * 1024);
    drop(db);

    let mut db = Database::open(&path, "test", inputs(&text), (), limits)?;
    assert_eq!(db.run(|db| tree(db, 0))?.as_str(), format!("0:{text}"));
    assert_eq!(db.statistics().stored_queries, 8);
    Ok(())
}
