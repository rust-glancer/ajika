use std::sync::Arc;

use ajika::{Database, DatabaseOptions, Output, Retention};
use anyhow::Result;

use super::{
    Db, Settings, Sources, declarations, diagnostics, inputs, retention, test_options, tree,
};

#[ajika::query]
fn repeated_output(db: &Db, output: Output<String>, consumer: u32) -> ajika::Result<String> {
    let value = output.value_ref(db)?;
    for _ in 0..100 {
        assert!(std::ptr::eq(value, output.value(db)?.as_ref()));
    }
    Ok(format!("{consumer}:{value}"))
}

#[ajika::query]
fn parallel_outputs(db: &Db) -> ajika::Result<Vec<String>> {
    let output = declarations(db)?.shape.clone();
    let values = db.parallel_map(&[0, 1, 2, 3], |db, consumer| {
        Ok(repeated_output(db, output.clone(), *consumer)?
            .as_ref()
            .clone())
    })?;
    Ok(values)
}

#[test]
fn repeated_output_reads_remain_dependencies_in_every_consumer() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    for (text, expected_executions) in [("one", 6), ("one ", 1), ("two", 6), ("two", 0)] {
        // Reopening also exercises validation with no resident values or validity state.
        let mut db = Database::open(&path, "test", inputs(text), (), test_options())?;
        let expected = (0..4)
            .map(|id| format!("{id}:{}", text.trim()))
            .collect::<Vec<_>>();
        assert_eq!(*db.run(parallel_outputs)?, expected);
        assert_eq!(db.statistics().executed, expected_executions, "{text:?}");
    }
    Ok(())
}

#[test]
fn shared_reads_survive_partial_replacement_and_are_reclaimed_with_the_last_caller() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for deferred in [false, true] {
        let path = directory
            .path()
            .join(if deferred { "deferred" } else { "eager" });
        let options = if deferred {
            test_options().defer_checkpoints(1024 * 1024)
        } else {
            test_options()
        };
        // All callers read the same text. Replace only half of them, leaving two different
        // observations of that input on disk at the same time.
        for (text, count, executions) in [("old", 12, 12), ("new", 6, 6), ("new", 12, 6)] {
            let mut db = Database::open(&path, "test", inputs(text), (), options.clone())?;
            db.run(|db| {
                for file in 0..count {
                    assert_eq!(tree(db, file)?.as_str(), format!("{file}:{text}"));
                }
                Ok(())
            })?;
            assert_eq!(db.statistics().executed, executions);
            db.collect()?;
            let disk = db.storage_statistics()?.expect("disk database");
            assert!(
                disk.record_bytes + disk.raw_value_bytes < db.statistics().stored_bytes,
                "the on-disk metadata including the dictionary should be smaller than storing each read inline"
            );
        }
        let mut db = Database::open(
            &path,
            "test",
            inputs("new"),
            (),
            DatabaseOptions::new(Retention {
                max_entries: 0,
                ..retention()
            }),
        )?;
        assert_eq!(db.collect()?, 12);
        assert_eq!(
            db.storage_statistics()?
                .expect("disk database")
                .record_bytes,
            0
        );
    }
    Ok(())
}

#[ajika::query(transient)]
fn transient_output(db: &Db) -> ajika::Result<Output<String>> {
    db.output("text", Settings::at(()).text(db)?)
}

#[test]
fn borrowed_output_survives_reconstructing_its_producer() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut db = Database::open(
        directory.path(),
        "test",
        inputs("hello"),
        (),
        test_options(),
    )?;
    let output = db.run(|db| Ok(transient_output(db)?.as_ref().clone()))?;

    db.run(|db| {
        let text = output.clone().value_ref(db)?;
        // The slot can be decoded on its own. Requesting the transient primary value
        // then reruns the producer and installs a new set of outputs in its memo.
        transient_output(db)?;
        assert_eq!(text, "hello");
        Ok(())
    })?;
    assert_eq!(db.statistics().decoded, 1);
    assert_eq!(db.statistics().executed, 1);
    Ok(())
}

#[ajika::query]
fn configuration(db: &Db) -> ajika::Result<u32> {
    Settings::at(()).configuration(db)
}

#[ajika::query]
fn configuration_label(db: &Db) -> ajika::Result<String> {
    Ok(format!("configuration: {}", configuration(db)?))
}

#[test]
fn saved_output_replays_its_producer_after_collection_and_reopen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    // Collecting this deferred checkpoint saves the family assignments and evicts the
    // producers. The handle will have to find its original implementation after restart.
    let options = DatabaseOptions::new(Retention {
        max_entries: 0,
        ..retention()
    })
    .defer_checkpoints(1024 * 1024);
    let mut db = Database::open(directory.path(), "test", inputs("first"), (), options)?;
    let output = db.run(declarations)?;
    db.run(configuration)?;
    let saved = wincode::serialize(&output.shape)?;
    assert!(db.collect()? > 0);
    assert_eq!(db.statistics().stored_queries, 0);
    drop(db);

    let mut db = Database::open(
        directory.path(),
        "test",
        inputs("second"),
        (),
        test_options(),
    )?;
    db.run(configuration)?;
    let handle: Output<String> = wincode::deserialize_exact(&saved)?;
    assert_eq!(db.run(|db| handle.value(db))?.as_str(), "second");
    Ok(())
}

#[test]
fn durability_skips_unaffected_subgraphs_but_never_skips_restart_validation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(
        &path,
        "test",
        Sources::default().capture(),
        (),
        test_options(),
    )?;
    db.run(configuration_label)?;

    for medium in [false, true] {
        db.update_inputs(|update| {
            if medium {
                Settings::at(()).set_file_revision(update, 1)
            } else {
                Settings::at(()).set_text(update, Arc::new("edited workspace source".to_owned()))
            }
        })?;
        let (value, events) =
            diagnostics::capture(tracing::Level::TRACE, || db.run(configuration_label));
        assert_eq!(value?.as_str(), "configuration: 0");
        events.assert_contains(
            "configuration_label",
            &[("message", "reuse"), ("reason", "durability")],
        );
        assert_eq!(db.statistics().validated, 0, "configuration is still valid");
        assert_eq!(db.statistics().metadata_reads, 0);
        assert_eq!(
            db.statistics().header_reads,
            1,
            "durability needs the offloaded header, not dependency validation"
        );
        assert_eq!(db.statistics().executed, 0);
    }

    db.replace_inputs(
        Sources {
            left: 7,
            ..Sources::default()
        }
        .capture(),
    );
    assert_eq!(db.run(configuration_label)?.as_str(), "configuration: 7");
    assert_eq!(db.statistics().executed, 2);
    drop(db);

    let mut db = Database::open(
        &path,
        "test",
        Sources::default().capture(),
        (),
        test_options(),
    )?;
    assert_eq!(db.run(configuration_label)?.as_str(), "configuration: 0");
    assert_eq!(db.statistics().executed, 2, "reopening captures new inputs");
    Ok(())
}

#[ajika::output]
struct OptionalPayload {
    #[output(optional)]
    text: String,
    // A required slot can also contain an absent payload, independently of optional emission.
    annotation: Option<String>,
}

#[ajika::query]
fn optional_payload(db: &Db) -> ajika::Result<OptionalPayload> {
    let text = Settings::at(()).text(db)?;
    let outputs = if text.is_empty() {
        OptionalPayload::new(db, None, None)?
    } else {
        OptionalPayload::new(db, Some(text), Some("present".to_owned()))?
    };
    Ok(outputs)
}

#[ajika::query]
fn optional_length(db: &Db) -> ajika::Result<usize> {
    let length = match &optional_payload(db)?.text {
        Some(text) => text.value(db)?.len(),
        None => 0,
    };
    Ok(length)
}

#[ajika::query]
fn selected_length(db: &Db, output: Output<String>) -> ajika::Result<usize> {
    Ok(output.value(db)?.len())
}

#[test]
fn removing_an_output_updates_its_consumers_and_reclaims_its_payload() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(
        &path,
        "test",
        inputs(&"x".repeat(200_000)),
        (),
        test_options(),
    )?;
    assert_eq!(*db.run(optional_length)?, 200_000);
    let selected = db.run(|db| Ok(optional_payload(db)?.text.clone().expect("emitted text")))?;
    assert_eq!(
        *db.run(|db| selected_length(db, selected.clone()))?,
        200_000
    );
    let old_bytes = db.statistics().stored_bytes;

    db.replace_inputs(inputs(""));
    let (value, events) = diagnostics::capture(tracing::Level::TRACE, || db.run(optional_length));
    assert_eq!(*value?, 0);
    events.assert_contains(
        "optional_payload",
        &[
            ("message", "output"),
            ("output", "text"),
            ("result", "removed"),
            ("version", "removed"),
        ],
    );
    assert_eq!(db.statistics().executed, 2);
    assert!(db.statistics().stored_bytes < old_bytes / 10);
    // A caller retaining just the removed handle fails, while callers selecting the new
    // set of output handles above can take its absent-output branch. The diagnostic identifies the lost slot.
    let (result, events) = diagnostics::capture(tracing::Level::DEBUG, || {
        db.run(|db| selected_length(db, selected))
    });
    assert!(result.is_err());
    events.assert_contains(
        "selected_length",
        &[
            ("message", "invalidated"),
            ("reason", "output_removed"),
            ("output", "text"),
        ],
    );
    db.run(|db| {
        let outputs = optional_payload(db)?;
        assert!(outputs.text.is_none());
        assert!(
            outputs.annotation.value(db)?.is_none(),
            "required slot still exists"
        );
        Ok(())
    })?;
    drop(db);

    let mut db = Database::open(&path, "test", inputs(""), (), test_options())?;
    assert_eq!(*db.run(optional_length)?, 0);
    assert_eq!(db.statistics().executed, 0);
    db.replace_inputs(inputs("restored"));
    assert_eq!(*db.run(optional_length)?, 8);
    Ok(())
}

// Deliberately omit registration. Direct typed calls work, but replaying this name from a saved
// parent's dependencies cannot prove validity. It must rerun the parent, not claim an input edit.
struct Unregistered;

impl ajika::Query for Unregistered {
    type Services = ();
    type Key = ();
    type Value = u32;
    const NAME: &'static str = "unregistered";

    fn execute(_: &Db, _: ()) -> ajika::Result<Arc<u32>> {
        Ok(Arc::new(7))
    }

    fn storage() -> ajika::Storage<u32> {
        ajika::Storage::persistent()
    }
}

#[ajika::query(forward)]
fn uses_unregistered(db: &Db) -> ajika::Result<Arc<u32>> {
    db.query::<Unregistered>(())
}

#[test]
fn unavailable_registrations_are_distinct_from_changed_values() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(&path, "test", inputs(""), (), test_options())?;
    assert_eq!(*db.run(uses_unregistered)?, 7);
    drop(db);
    let mut db = Database::open(&path, "test", inputs(""), (), test_options())?;
    let (result, events) =
        diagnostics::capture(tracing::Level::DEBUG, || db.run(uses_unregistered));
    assert_eq!(*result?, 7);
    assert_eq!(db.statistics().executed, 1);
    events.assert_contains(
        "uses_unregistered",
        &[
            ("message", "invalidated"),
            ("reason", "registration_unavailable"),
            ("dependency", "unregistered"),
        ],
    );
    Ok(())
}

#[test]
fn deferred_output_removal_hides_the_disk_slot_until_collection_reclaims_it() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut db = Database::open(
        directory.path(),
        "test",
        inputs("old"),
        (),
        test_options().defer_checkpoints(1024 * 1024),
    )?;
    let old = db.run(|db| Ok(optional_payload(db)?.text.clone().expect("emitted slot")))?;
    assert_eq!(*db.run(optional_length)?, 3);
    db.collect()?;
    let before = db.storage_statistics()?.expect("disk database").values;

    db.replace_inputs(inputs(""));
    assert_eq!(*db.run(optional_length)?, 0);
    assert!(db.run(|db| old.value(db)).is_err());
    assert_eq!(
        db.storage_statistics()?.expect("disk database").values,
        before
    );

    db.collect()?;
    // Collection deletes the old output from disk. The next request can still reuse both queries.
    assert_eq!(
        db.storage_statistics()?.expect("disk database").values,
        before - 1
    );
    assert_eq!(*db.run(optional_length)?, 0);
    assert_eq!(db.statistics().executed, 0);
    Ok(())
}

#[derive(wincode::SchemaRead, wincode::SchemaWrite)]
struct Label(String);

impl PartialEq for Label {
    fn eq(&self, other: &Self) -> bool {
        self.0.trim() == other.0.trim()
    }
}

#[ajika::query]
fn label(db: &Db) -> ajika::Result<Label> {
    Ok(Label(Settings::at(()).text(db)?.as_ref().clone()))
}

#[test]
fn equal_pending_values_can_have_different_encoded_lengths() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let options = test_options().defer_checkpoints(1024 * 1024);
    let mut db = Database::open(directory.path(), "test", inputs("one"), (), options.clone())?;
    db.run(label)?;
    db.replace_inputs(inputs(" one "));
    db.run(label)?;
    assert_eq!(db.statistics().unchanged, 1);
    db.collect()?;
    drop(db);
    let mut db = Database::open(directory.path(), "test", inputs(" one "), (), options)?;
    assert_eq!(db.run(label)?.0.trim(), "one");
    assert_eq!(db.statistics().executed, 0);
    Ok(())
}
