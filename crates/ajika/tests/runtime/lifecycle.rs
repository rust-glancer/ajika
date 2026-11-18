use std::{
    panic,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use ajika::{Database, DatabaseOptions, Inputs, QueryCycle, ResultExt as _, Retention};
use anyhow::Result;

use super::{Sources, inputs, leaf, retention, symbols, test_options, tree};

struct Control {
    enabled: AtomicBool,
    entered: Barrier,
}

struct TestServices(Arc<Control>);

impl ajika::Services for TestServices {}

type Db<'a> = ajika::Db<'a, TestServices>;

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
fn wait_for_cancellation(db: &Db) -> ajika::Result<u32> {
    if OperationControl::at(()).enabled(db)? {
        db.services().0.entered.wait();
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
        TestServices(control.clone()),
        test_options().parallelism(2),
    )?;
    let cancellation = db.cancellation();
    let result = thread::scope(|scope| {
        scope.spawn(|| {
            control.entered.wait();
            cancellation.cancel();
        });
        db.run(|db| db.parallel_map(&[(); 32], |db, _| wait_for_cancellation(db)))
    });
    assert!(result.expect_err("operation was cancelled").is_cancelled());
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
fn may_panic(db: &Db) -> ajika::Result<u32> {
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
    let mut db = Database::memory(
        control_inputs(&control),
        TestServices(control.clone()),
        test_options(),
    );
    let panic = panic::catch_unwind(panic::AssertUnwindSafe(|| db.run(may_panic)));
    assert!(panic.is_err());
    control.enabled.store(false, Ordering::Release);
    db.replace_inputs(control_inputs(&control));
    assert_eq!(*db.run(may_panic)?, 42);
    assert_eq!(db.statistics().executed, 1);
    Ok(())
}

#[ajika::query(forward)]
fn mutually_recursive(db: &Db, key: u32) -> ajika::Result<Arc<u32>> {
    if OperationControl::at(()).enabled(db)? {
        db.services().0.entered.wait();
        return recursive_peer(db, 1 - key).context("building declarations");
    }
    Ok(Arc::new(key))
}

#[ajika::query(forward)]
fn recursive_peer(db: &Db, key: u32) -> ajika::Result<Arc<u32>> {
    mutually_recursive(db, key).context("resolving associated items")
}

fn cycle_in(error: &ajika::Error) -> &QueryCycle {
    match error.kind() {
        ajika::ErrorKind::Cycle(cycle) => cycle,
        _ => panic!("expected structured cycle"),
    }
}

#[test]
fn a_cross_worker_cycle_finishes_and_does_not_poison_the_next_operation() -> Result<()> {
    let control = Arc::new(Control {
        enabled: AtomicBool::new(true),
        entered: Barrier::new(2),
    });
    let mut db = Database::memory(
        control_inputs(&control),
        TestServices(control.clone()),
        test_options().parallelism(2),
    );
    let result: ajika::Result<()> = db.run(|db| {
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
    let report = format!("{:#}", anyhow::Error::new(error.clone()));
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
    let mut db = Database::memory(ajika::Inputs::default(), (), test_options().parallelism(2));
    let entered = Barrier::new(2);
    let panic = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        db.run(|db| {
            db.parallel_map(&[0, 1], |db, key| -> ajika::Result<()> {
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
    assert_eq!(db.run(|_| { Ok(42) })?, 42);
    Ok(())
}

#[test]
fn a_failed_operation_discards_new_work_but_keeps_the_previous_checkpoint() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(&path, "test", inputs("old"), (), test_options())?;
    db.run(|db| symbols(db, 1))?;
    db.replace_inputs(inputs("new"));
    #[derive(Debug, thiserror::Error)]
    #[error("candidate rejected with code {0}")]
    struct Rejected(u32);
    let failed: ajika::Result<()> = db.run(|db| {
        symbols(db, 1)?;
        Err(ajika::Error::application(Rejected(42)))
    });
    let error = failed.expect_err("host rejected the candidate");
    let ajika::ErrorKind::Application(source) = error.kind() else {
        panic!("expected application rejection");
    };
    assert_eq!(
        source
            .downcast_ref::<Rejected>()
            .expect("application rejection")
            .0,
        42
    );
    drop(db);

    let mut db = Database::open(&path, "test", inputs("old"), (), test_options())?;
    assert_eq!(db.run(|db| symbols(db, 1))?.as_str(), "symbols:1:old");
    assert_eq!(db.statistics().executed, 0);
    db.replace_inputs(inputs("new"));
    assert_eq!(db.run(|db| symbols(db, 1))?.as_str(), "symbols:1:new");
    Ok(())
}

#[test]
fn a_new_schema_epoch_and_explicit_invalidation_start_fresh() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(
        &path,
        "one",
        Sources::default().capture(),
        (),
        test_options(),
    )?;
    db.run(|db| leaf(db, 1))?;
    drop(db);
    let mut db = Database::open(
        &path,
        "two",
        Sources::default().capture(),
        (),
        test_options(),
    )?;
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
    let mut db = Database::open(&path, "test", inputs(&large), (), test_options())?;
    db.run(|db| tree(db, 1))?;
    let old_bytes = db.statistics().stored_bytes;
    drop(db);

    // Reopening must recover a large value from its independently stored payload.
    let mut db = Database::open(&path, "test", inputs(&large), (), test_options())?;
    assert_eq!(db.run(|db| tree(db, 1))?.as_str(), format!("1:{large}"));
    assert_eq!(db.statistics().executed, 0);
    db.replace_inputs(inputs("small"));
    db.run(|db| tree(db, 1))?;
    assert!(db.statistics().stored_bytes < old_bytes / 10);
    drop(db);
    let mut db = Database::open(&path, "test", inputs("small"), (), test_options())?;
    assert_eq!(db.run(|db| tree(db, 1))?.as_str(), "1:small");
    assert_eq!(db.statistics().executed, 0);
    Ok(())
}

#[test]
fn recently_used_queries_survive_eviction_after_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(&path, "test", Inputs::default(), (), test_options())?;

    // Persist distinct creation times, then make the older query the most recently used.
    // The pauses separate millisecond timestamps without relying on an idle-expiry deadline.
    db.run(|db| leaf(db, 1))?;
    thread::sleep(Duration::from_millis(2));
    db.run(|db| leaf(db, 2))?;
    thread::sleep(Duration::from_millis(2));
    db.run(|db| leaf(db, 1))?;
    assert_eq!(db.statistics().executed, 0);
    assert_eq!(db.collect()?, 0);
    drop(db);

    let limits = Retention {
        max_entries: 1,
        ..retention()
    };
    let mut db = Database::open(
        &path,
        "test",
        Inputs::default(),
        (),
        DatabaseOptions::new(limits),
    )?;
    assert_eq!(db.collect()?, 1);
    assert_eq!(*db.run(|db| leaf(db, 1))?, 1);
    assert_eq!(
        db.statistics().executed,
        0,
        "the recently read query survived"
    );
    assert_eq!(*db.run(|db| leaf(db, 2))?, 2);
    assert_eq!(db.statistics().executed, 1, "the older query was evicted");
    Ok(())
}

#[test]
fn retention_also_reclaims_obsolete_payloads_between_operations() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let text = "x".repeat(256 * 1024);
    let limits = Retention {
        max_entries: 2,
        ..retention()
    };
    let mut db = Database::open(
        &path,
        "test",
        inputs(&text),
        (),
        DatabaseOptions::new(limits).compression(ajika::Compression::None),
    )?;
    // Writing all results together puts them in one pack. Evicting six leaves enough dead
    // space to trigger automatic compaction while two results still need their payloads.
    db.run(|db| {
        for file in 0..8 {
            tree(db, file)?;
        }
        Ok(())
    })?;
    let storage = db.storage_statistics()?.expect("disk storage");
    assert_eq!(storage.values, 2);
    assert_eq!(storage.dead_pack_bytes, 0);
    assert!(storage.pack_file_bytes < 1024 * 1024);
    drop(db);

    let mut db = Database::open(
        &path,
        "test",
        inputs(&text),
        (),
        DatabaseOptions::new(limits),
    )?;
    db.run(|db| {
        for file in 0..8 {
            assert_eq!(tree(db, file)?.as_str(), format!("{file}:{text}"));
        }
        Ok(())
    })?;
    assert_eq!(
        db.statistics().executed,
        6,
        "only evicted results run again"
    );
    assert_eq!(db.statistics().stored_queries, 2);
    Ok(())
}

#[test]
fn deferred_runs_coalesce_and_release_their_values_on_collect() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let options = test_options().defer_checkpoints(1024 * 1024);
    let mut db = Database::open(directory.path(), "test", inputs("one"), (), options.clone())?;
    for text in ["one", "two"] {
        db.replace_inputs(inputs(text));
        let first = db.run(|db| symbols(db, 1))?;
        let second = db.run(|db| symbols(db, 1))?;
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.as_str(), format!("symbols:1:{text}"));
        let stats = db.statistics();
        assert_eq!(
            (stats.executed, stats.decoded, stats.header_reads),
            (0, 0, 0)
        );
        assert_eq!(stats.pending_queries, 2);
    }
    assert_eq!(db.storage_statistics()?.expect("disk database").records, 0);
    let latest = db.run(|db| Ok(Arc::downgrade(&symbols(db, 1)?)))?;
    db.collect()?;
    assert!(latest.upgrade().is_none());
    let stats = db.statistics();
    assert_eq!((stats.pending_queries, stats.pending_bytes), (0, 0));
    assert_eq!(stats.metadata_writes, 2);
    // Closing with another edit pending leaves the last disk checkpoint reusable.
    db.replace_inputs(inputs("three"));
    db.run(|db| symbols(db, 1))?;
    drop(db);
    let mut db = Database::open(directory.path(), "test", inputs("two"), (), options)?;
    assert_eq!(db.run(|db| symbols(db, 1))?.as_str(), "symbols:1:two");
    assert_eq!(db.statistics().executed, 0);
    Ok(())
}

#[test]
fn pending_byte_limit_writes_an_oversized_operation_before_returning() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let inputs = inputs(&"x".repeat(4096));
    let options = test_options().defer_checkpoints(1024);
    let mut db = Database::open(
        directory.path(),
        "test",
        inputs.clone(),
        (),
        options.clone(),
    )?;
    db.run(|db| leaf(db, 0))?;
    assert_eq!(db.statistics().pending_queries, 1);
    db.run(|db| symbols(db, 1))?;
    let stats = db.statistics();
    assert_eq!(stats.pending_queries, 0);
    assert_eq!(stats.stored_queries, 3);
    assert_eq!(stats.metadata_writes, 3);

    // A pressure drain commits the batch even if the host closes before the next collect.
    drop(db);
    let mut db = Database::open(directory.path(), "test", inputs, (), options)?;
    db.run(|db| symbols(db, 1))?;
    assert_eq!(db.statistics().executed, 0);
    Ok(())
}

#[test]
fn a_deferred_batch_can_retry_with_compression_after_a_capacity_refusal() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let options = test_options().defer_checkpoints(16 * 1024 * 1024);
    let db = Database::open(
        directory.path(),
        "test",
        Inputs::default(),
        (),
        options.clone(),
    )?;
    let base = db
        .storage_statistics()?
        .expect("disk database")
        .allocated_bytes;
    drop(db);
    let mut db = Database::open(
        directory.path(),
        "test",
        inputs(&"x".repeat(8 * 1024 * 1024)),
        (),
        options
            .max_physical_bytes(base + 4 * 1024 * 1024)
            .compression(ajika::Compression::None),
    )?;
    let answer = db.run(|db| tree(db, 1))?;
    assert!(
        matches!(db.collect().expect_err("raw value exceeds the budget").kind(),
        ajika::ErrorKind::Persistence(error) if error.kind() == ajika::PersistenceErrorKind::Capacity)
    );
    assert_eq!(db.statistics().stored_queries, 0);
    assert!(Arc::ptr_eq(&answer, &db.run(|db| tree(db, 1))?));
    db.set_compression(ajika::Compression::Lz4);
    db.collect()?;
    assert_eq!(db.statistics().pending_bytes, 0);
    assert_eq!(db.statistics().stored_queries, 1);
    Ok(())
}

#[test]
fn deferred_checkpoints_can_free_disk_space_before_retrying() -> Result<()> {
    // Each payload is 4 MiB. The smaller RAM budget triggers a write during run().
    for (pending_budget, expect_automatic) in [(8 * 1024 * 1024, false), (1024 * 1024, true)] {
        let directory = tempfile::tempdir()?;
        let inputs = inputs(&"x".repeat(4 * 1024 * 1024));
        let options = test_options().compression(ajika::Compression::None);
        let mut db = Database::open(directory.path(), "test", inputs.clone(), (), options)?;
        db.run(|cx| tree(cx, 1))?;
        let used = db
            .storage_statistics()?
            .expect("disk database")
            .allocated_bytes;
        drop(db);

        // Open the old cache with a smaller retention limit. Its old pack must be evicted
        // before another full-size payload fits on disk.
        let options = DatabaseOptions::new(Retention {
            max_entries: 1,
            ..retention()
        })
        .compression(ajika::Compression::None)
        .max_physical_bytes(used + 2 * 1024 * 1024)
        .defer_checkpoints(pending_budget);
        let mut db = Database::open(directory.path(), "test", inputs, (), options)?;
        db.run(|cx| tree(cx, 2))?;
        if expect_automatic {
            assert_eq!(db.statistics().collected, 1);
        } else {
            assert_eq!(db.collect()?, 1);
        }
        assert_eq!(db.statistics().pending_queries, 0);
        assert_eq!(db.storage_statistics()?.expect("disk database").records, 1);
        db.run(|cx| tree(cx, 2))?;
        assert_eq!(db.statistics().executed, 0);
    }
    Ok(())
}

#[test]
fn collection_can_trim_pending_work_to_fit_on_disk() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let options = DatabaseOptions::new(Retention {
        max_entries: 1,
        ..retention()
    })
    .compression(ajika::Compression::None)
    .defer_checkpoints(16 * 1024 * 1024);
    let db = Database::open(
        directory.path(),
        "test",
        Inputs::default(),
        (),
        options.clone(),
    )?;
    let used = db
        .storage_statistics()?
        .expect("disk database")
        .allocated_bytes;
    drop(db);

    // Only one result should survive collection. The disk has room for one, but writing
    // both before choosing which to keep would exhaust its budget.
    let options = options.max_physical_bytes(used + 6 * 1024 * 1024);
    let mut db = Database::open(
        directory.path(),
        "test",
        inputs(&"x".repeat(4 * 1024 * 1024)),
        (),
        options,
    )?;
    db.run(|cx| {
        tree(cx, 1)?;
        tree(cx, 2)?;
        Ok(())
    })?;
    assert_eq!(db.collect()?, 1);
    assert_eq!(db.statistics().pending_queries, 0);
    assert_eq!(db.storage_statistics()?.expect("disk database").records, 1);
    Ok(())
}
