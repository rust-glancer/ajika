use std::{sync::Arc, time::Duration};

use ajika::{Database, Inputs, Result, Retention};

mod associated;
mod attributes;
mod diagnostics;
mod interner;
mod invalidation;
mod lifecycle;
mod managed_inputs;
mod session;

#[derive(Default)]
struct Sources {
    text: String,
    choose_left: bool,
    left: u32,
    right: u32,
}

impl Sources {
    fn capture(self) -> Inputs {
        Inputs::default()
            .edit(|update| {
                let settings = Settings::at(());
                settings.set_text(update, Arc::new(self.text))?;
                settings.set_choose_left(update, self.choose_left)?;
                settings.set_configuration(update, self.left)?;
                settings.set_file_revision(update, 0)?;
                Number::at(true).set_value(update, self.left)?;
                Number::at(false).set_value(update, self.right)
            })
            .expect("test inputs serialize")
    }
}

type Db<'a> = ajika::Db<'a, ()>;
type QueryResult<T> = Result<T>;
type SharedQueryResult<T> = Result<Arc<T>>;

fn inputs(text: &str) -> Inputs {
    Sources {
        text: text.to_owned(),
        ..Sources::default()
    }
    .capture()
}
fn retention() -> Retention {
    Retention {
        max_entries: 256,
        max_bytes: 16 * 1024 * 1024,
        max_idle: Duration::from_secs(3600),
    }
}

#[ajika::input(key = ())]
struct Settings {
    text: Arc<String>,
    choose_left: bool,
    #[input(durability = High)]
    configuration: u32,
    #[input(durability = Medium)]
    file_revision: u32,
}

#[ajika::input(key = bool)]
struct Number {
    value: u32,
}

#[ajika::query]
fn tree(db: &Db, file: u32) -> Result<String> {
    let text = Settings::at(()).text(db)?;
    Ok(format!(
        "{file}:{}",
        text.split_whitespace().collect::<String>()
    ))
}

#[ajika::query]
fn symbols(db: &Db, file: u32) -> Result<String> {
    Ok(format!("symbols:{}", tree(db, file)?))
}

#[test]
fn repeated_calls_share_values_and_same_revision_validity_survives_release() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut db = Database::open(
        directory.path().join("queries"),
        "test",
        inputs("fn main() {}"),
        (),
        retention(),
    )?;
    let (weak, events) = diagnostics::capture(tracing::Level::TRACE, || {
        db.run(|db| {
            let value = symbols(db, 1)?;
            for _ in 0..100 {
                assert!(Arc::ptr_eq(&value, &symbols(db, 1)?));
            }
            Ok(Arc::downgrade(&value))
        })
    });
    let weak = weak?;
    let first_operation = events.operation(1);
    events.assert_contains(
        "symbols",
        &[("message", "reuse"), ("reason", "resident_value")],
    );
    assert!(
        weak.upgrade().is_none(),
        "operation released its decoded working set"
    );
    assert_eq!(db.statistics().executed, 2);
    assert_eq!(
        db.statistics().decoded,
        0,
        "fresh values do not round-trip through disk"
    );

    // Idle maintenance can move metadata without losing same-revision validity.
    db.collect()?;
    let (value, events) =
        diagnostics::capture(tracing::Level::TRACE, || db.run(|db| symbols(db, 1)));
    assert_ne!(first_operation, events.operation(1));
    events.assert_contains(
        "symbols",
        &[("message", "reuse"), ("reason", "validity_proof")],
    );
    assert_eq!(value?.as_str(), "symbols:1:fnmain(){}");
    assert_eq!(db.statistics().executed, 0);
    assert_eq!(db.statistics().validated, 0);
    assert_eq!(db.statistics().metadata_reads, 0);
    assert_eq!(
        db.statistics().header_reads,
        1,
        "reload the output metadata without revalidating dependencies"
    );
    assert_eq!(
        db.statistics().decoded,
        1,
        "only the requested result is decoded"
    );
    Ok(())
}

#[test]
fn a_space_edit_stops_at_equal_output_and_survives_reopening() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(&path, "test", inputs("fn main() {}"), (), retention())?;
    db.run(|db| symbols(db, 1))?;
    db.replace_inputs(inputs("fn  main() {}"));
    let (result, events) =
        diagnostics::capture(tracing::Level::TRACE, || db.run(|db| symbols(db, 1)));
    result?;
    events.operation(2);
    events.assert_contains(
        "tree",
        &[
            ("message", "invalidated"),
            ("reason", "input_stamp_changed"),
            ("dependency", "runtime::Settings::text"),
        ],
    );
    events.assert_contains(
        "tree",
        &[("message", "execute"), ("reason", "dependency_invalid")],
    );
    events.assert_contains(
        "tree",
        &[
            ("message", "output"),
            ("result", "equal"),
            ("evidence", "comparison"),
            ("version", "preserved"),
        ],
    );
    events.assert_contains(
        "symbols",
        &[("message", "reuse"), ("reason", "dependencies_valid")],
    );
    assert_eq!(db.statistics().executed, 1, "tree ran; symbols did not");
    drop(db);

    let mut db = Database::open(&path, "test", inputs("fn  main() {}"), (), retention())?;
    db.run(|db| symbols(db, 1))?;
    assert_eq!(db.statistics().executed, 0);
    assert_eq!(db.statistics().decoded, 1);
    assert!(
        db.statistics().validated > 0,
        "restart verifies inputs again"
    );
    Ok(())
}

#[test]
fn memory_storage_retains_typed_values_without_encoding() -> Result<()> {
    let mut db = Database::memory(inputs("hello"), (), retention());
    let weak = db.run(|db| Ok(Arc::downgrade(&tree(db, 1)?)))?;
    assert!(
        weak.upgrade().is_some(),
        "resident backend owns the typed value"
    );
    db.run(|db| tree(db, 1))?;
    assert_eq!(db.statistics().executed, 0);
    assert_eq!(db.statistics().decoded, 0);
    assert_eq!(db.statistics().bytes_written, 0);
    Ok(())
}

// Syntax deliberately has neither equality nor serialization. Its dependency
// record still lets a stored parent validate without recreating the syntax value.
struct Syntax(String);
#[ajika::query(transient)]
fn parse(db: &Db) -> Result<Syntax> {
    Ok(Syntax(Settings::at(()).text(db)?.to_string()))
}

#[ajika::query(forward)]
fn forwarded_text(db: &Db) -> SharedQueryResult<String> {
    Settings::at(()).text(db)
}

#[ajika::query(transient, forward)]
fn forwarded_syntax(db: &Db) -> SharedQueryResult<Syntax> {
    parse(db)
}

#[test]
fn forwarding_reuses_input_and_non_cloneable_query_values() -> Result<()> {
    let mut db = Database::memory(inputs("hello"), (), retention());
    db.run(|db| {
        let text = Settings::at(()).text(db)?;
        assert!(Arc::ptr_eq(&text, &forwarded_text(db)?));
        let syntax = parse(db)?;
        assert!(Arc::ptr_eq(&syntax, &forwarded_syntax(db)?));
        Ok(())
    })
}

#[ajika::query]
fn parsed_length(db: &Db) -> Result<usize> {
    Ok(parse(db)?.0.len())
}

#[test]
fn transient_intermediates_keep_replayable_dependency_records() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(&path, "test", inputs("hello"), (), retention())?;
    db.run(parsed_length)?;
    drop(db);
    let mut db = Database::open(&path, "test", inputs("hello"), (), retention())?;
    assert_eq!(*db.run(parsed_length)?, 5);
    assert_eq!(db.statistics().executed, 0);
    assert_eq!(db.statistics().decoded, 1);
    let (result, events) = diagnostics::capture(tracing::Level::TRACE, || db.run(parse));
    result?;
    events.operation(1);
    events.assert_contains(
        "parse",
        &[("message", "execute"), ("reason", "value_unavailable")],
    );
    events.assert_contains(
        "parse",
        &[
            ("message", "output"),
            ("result", "equal"),
            ("evidence", "validity_proof"),
            ("version", "preserved"),
        ],
    );
    assert_eq!(
        db.statistics().executed,
        1,
        "syntax materializes only when requested"
    );
    assert_eq!(
        db.statistics().bytes_written,
        0,
        "materialization keeps stored versions"
    );
    assert_eq!(*db.run(parsed_length)?, 5);
    assert_eq!(db.statistics().header_reads, 1);
    assert_eq!(db.statistics().metadata_reads, 0);
    assert_eq!(db.statistics().executed, 0);
    // After an edit, the previous syntax cannot be compared. A fresh version must not be
    // described as proof of unequal values, even though it makes callers reconsider their work.
    db.replace_inputs(inputs("world"));
    let (result, events) = diagnostics::capture(tracing::Level::TRACE, || db.run(parse));
    result?;
    events.assert_contains(
        "parse",
        &[
            ("message", "output"),
            ("result", "comparison_unavailable"),
            ("version", "advanced"),
        ],
    );
    Ok(())
}

#[ajika::output]
struct DeclarationOutputs {
    shape: String,
    location: usize,
}
#[ajika::query]
fn declarations(db: &Db) -> Result<DeclarationOutputs> {
    let text = Settings::at(()).text(db)?;
    DeclarationOutputs::new(
        db,
        text.split_whitespace().collect::<String>(),
        Arc::new(text.len()),
    )
}
#[ajika::query(forward)]
fn semantic(db: &Db) -> Result<Arc<String>> {
    declarations(db)?.shape.value(db)
}
#[ajika::query(forward)]
fn navigation(db: &Db) -> Result<Arc<usize>> {
    declarations(db)?.location.value(db)
}

#[test]
fn named_outputs_change_independently() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut db = Database::open(
        directory.path().join("queries"),
        "test",
        inputs("fn main() {}"),
        (),
        retention(),
    )?;
    db.run(|db| {
        semantic(db)?;
        navigation(db)
    })?;
    db.replace_inputs(inputs("fn  main() {}"));
    let (result, events) = diagnostics::capture(tracing::Level::TRACE, || {
        db.run(|db| {
            assert_eq!(semantic(db)?.as_str(), "fnmain(){}");
            navigation(db)
        })
    });
    result?;
    events.assert_contains(
        "declarations",
        &[
            ("message", "output"),
            ("output", "shape"),
            ("result", "equal"),
            ("version", "preserved"),
        ],
    );
    events.assert_contains(
        "declarations",
        &[
            ("message", "output"),
            ("output", "location"),
            ("result", "unequal"),
            ("version", "advanced"),
        ],
    );
    events.assert_contains(
        "navigation",
        &[
            ("message", "invalidated"),
            ("reason", "dependency_version_changed"),
            ("output", "location"),
        ],
    );
    assert_eq!(
        db.statistics().executed,
        2,
        "producer and navigation ran; semantics stayed green"
    );
    let outputs = db.run(declarations)?;
    db.run(|db| {
        assert_eq!(outputs.shape.value(db)?.as_str(), "fnmain(){}");
        assert_eq!(*outputs.location.value(db)?, "fn  main() {}".len());
        Ok(())
    })?;
    assert_eq!(
        db.statistics().header_reads,
        1,
        "two named outputs share one metadata read"
    );
    assert_eq!(db.statistics().executed, 0);
    Ok(())
}

#[ajika::query]
fn choose(db: &Db) -> QueryResult<u32> {
    if Settings::at(()).choose_left(db)? {
        return Number::at(true).value(db);
    }
    Number::at(false).value(db)
}

#[test]
fn equal_branch_changes_replace_old_dependencies() -> Result<()> {
    let mut db = Database::memory(
        Sources {
            choose_left: true,
            left: 7,
            right: 7,
            ..Sources::default()
        }
        .capture(),
        (),
        retention(),
    );
    assert_eq!(*db.run(choose)?, 7);
    db.replace_inputs(
        Sources {
            left: 7,
            right: 7,
            ..Sources::default()
        }
        .capture(),
    );
    let (result, events) = diagnostics::capture(tracing::Level::DEBUG, || db.run(choose));
    result?;
    events.assert_contains(
        "choose",
        &[
            ("message", "invalidated"),
            ("reason", "input_stamp_changed"),
            ("dependency", "runtime::Settings::choose_left"),
        ],
    );
    assert_eq!(
        events
            .0
            .iter()
            .filter(|event| event["message"] == "invalidated")
            .count(),
        1,
        "validation reports only its first failed read"
    );
    assert_eq!(db.statistics().executed, 1);
    assert_eq!(
        db.statistics().compared,
        0,
        "memory storage compares typed values without decoding"
    );
    db.replace_inputs(
        Sources {
            left: 99,
            right: 7,
            ..Sources::default()
        }
        .capture(),
    );
    assert_eq!(*db.run(choose)?, 7);
    assert_eq!(
        db.statistics().executed,
        0,
        "the unused old branch is no longer a dependency"
    );
    Ok(())
}

#[ajika::query]
fn leaf(_db: &Db, key: u32) -> Result<u32> {
    Ok(key)
}
#[ajika::query]
fn parallel_root(db: &Db, _key: u32) -> Result<u32> {
    let children = db.parallel_map(&(0..16).collect::<Vec<_>>(), |db, key| leaf(db, *key))?;
    Ok(children.iter().map(|value| **value).sum())
}

#[test]
fn nested_parallel_queries_share_work_at_one_two_and_four_workers() -> Result<()> {
    for workers in [1, 2, 4] {
        let mut db = Database::memory(inputs(""), (), retention());
        db.set_parallelism(workers);
        let (values, events) = diagnostics::capture(tracing::Level::DEBUG, || {
            db.run(|db| {
                db.parallel_map(&(0..32).collect::<Vec<_>>(), |db, key| {
                    parallel_root(db, *key)
                })
            })
        });
        let values = values?;
        events.operation(1);
        assert_eq!(
            events.0.len(),
            48,
            "one execution event per query, including workers"
        );
        assert!(values.iter().all(|value| **value == 120));
        assert_eq!(
            db.statistics().executed,
            48,
            "32 roots share the same 16 children"
        );
    }
    Ok(())
}
