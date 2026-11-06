use std::sync::Arc;

use ajika::{Database, Output};
use anyhow::Result;

use super::{Db, Settings, Sources, declarations, diagnostics, inputs, retention};

#[ajika::query]
fn repeated_output(db: &Db, output: Output<String>, consumer: u32) -> ajika::Result<String> {
    let value = output.value(db)?;
    for _ in 0..100 {
        assert!(Arc::ptr_eq(&value, &output.value(db)?));
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
        let mut db = Database::open(&path, "test", inputs(text), (), retention())?;
        let expected = (0..4)
            .map(|id| format!("{id}:{}", text.trim()))
            .collect::<Vec<_>>();
        assert_eq!(*db.run(parallel_outputs)?, expected);
        assert_eq!(db.statistics().executed, expected_executions, "{text:?}");
    }
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
fn durability_skips_unaffected_subgraphs_but_never_skips_restart_validation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(&path, "test", Sources::default().capture(), (), retention())?;
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

    let mut db = Database::open(&path, "test", Sources::default().capture(), (), retention())?;
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
    let mut db = Database::open(&path, "test", inputs(&"x".repeat(200_000)), (), retention())?;
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

    let mut db = Database::open(&path, "test", inputs(""), (), retention())?;
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
    let mut db = Database::open(&path, "test", inputs(""), (), retention())?;
    assert_eq!(*db.run(uses_unregistered)?, 7);
    drop(db);
    let mut db = Database::open(&path, "test", inputs(""), (), retention())?;
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
