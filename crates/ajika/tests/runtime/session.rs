use std::sync::Arc;

use ajika::{Database, DatabaseOptions, Inputs, Lifetime, Output, Stamp};
use anyhow::Result;

use super::{retention, test_options};

#[derive(Clone)]
struct Source {
    text: &'static str,
    lifetime: Lifetime,
}

#[ajika::input(key = ())]
struct Document {
    #[input(stamp = Source::stamp, lifetime = Source::lifetime)]
    source: Source,
}

impl Source {
    fn stamp(&self) -> ajika::Result<Stamp> {
        Ok(Stamp::of(self.text.as_bytes()))
    }

    fn lifetime(&self) -> Lifetime {
        self.lifetime
    }

    fn capture(self) -> Inputs {
        Inputs::default()
            .edit(|update| Document::at(()).set_source(update, self))
            .expect("source fingerprint")
    }
}

type Db<'a> = ajika::Db<'a, ()>;
type Outputs = (Output<String>, Output<String>, Output<Vec<u8>>);

#[ajika::query]
fn document(db: &Db) -> ajika::Result<Outputs> {
    let text = Arc::new(Document::at(()).source(db)?.text.to_owned());
    Ok((
        db.output("meaning", Arc::new(text.split_whitespace().collect()))?,
        db.output("locations", text)?,
        db.output("unchanged-large-output", Arc::new(vec![7; 64 * 1024]))?,
    ))
}

#[ajika::query]
fn answer(db: &Db) -> ajika::Result<String> {
    Ok(format!("answer:{}", document(db)?.0.value(db)?))
}

#[ajika::query]
fn word_length(db: &Db, index: usize) -> ajika::Result<usize> {
    Ok(Document::at(())
        .source(db)?
        .text
        .split_whitespace()
        .nth(index)
        .map_or(0, str::len))
}

#[test]
fn all_callers_inherit_a_shared_inputs_new_lifetime() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = |lifetime| {
        Source {
            text: "a bb ccc dddd",
            lifetime,
        }
        .capture()
    };
    let read = |cx: &Db| {
        for i in 0..4 {
            assert_eq!(*word_length(cx, i)?, i + 1);
        }
        Ok(())
    };

    let mut db = Database::open(
        directory.path(),
        "test",
        input(Lifetime::Persistent),
        (),
        test_options(),
    )?;
    db.run(read)?;
    drop(db);

    // Opening an editor buffer with identical text changes its lifetime but preserves
    // its stamp. Every caller now belongs to the session, including those reusing a check.
    let mut db = Database::open(
        directory.path(),
        "test",
        input(Lifetime::Session),
        (),
        test_options(),
    )?;
    db.run(read)?;
    assert_eq!(db.statistics().executed, 0);
    assert_eq!(db.statistics().session_queries, 4);
    db.replace_inputs(input(Lifetime::Persistent));
    db.run(read)?;
    assert_eq!(db.statistics().executed, 0);
    assert_eq!(db.statistics().session_queries, 0);
    Ok(())
}

#[ajika::input(key = (), lifetime = Operation)]
struct Request {
    label: String,
}

#[ajika::query]
fn request_label(db: &Db) -> ajika::Result<String> {
    Request::at(()).label(db)
}

#[test]
fn one_checkpoint_retains_each_lifetime_in_its_own_store() -> Result<()> {
    for disk in [false, true] {
        let directory = tempfile::tempdir()?;
        let inputs = Source {
            text: "edited",
            lifetime: Lifetime::Session,
        }
        .capture()
        .edit(|update| Request::at(()).set_label(update, "request".to_owned()))?;
        let mut db = if disk {
            Database::open(directory.path(), "test", inputs, (), test_options())?
        } else {
            Database::memory(inputs, (), test_options())
        };

        // One checkpoint contains persistent, session and operation work. On the next request,
        // only the operation result needs execution; the other answers keep their own backing.
        for expected_executions in [4, 1] {
            db.run(|cx| {
                assert_eq!(*super::leaf(cx, 7)?, 7);
                assert_eq!(&*answer(cx)?, "answer:edited");
                assert_eq!(&*request_label(cx)?, "request");
                Ok(())
            })?;
            assert_eq!(db.statistics().executed, expected_executions);
            assert_eq!(db.statistics().stored_queries, 1);
            assert_eq!(db.statistics().session_queries, 2);
        }
    }
    Ok(())
}

#[test]
fn editor_results_survive_requests_and_save_without_recomputation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(
        &path,
        "test",
        Source {
            text: "one",
            lifetime: Lifetime::Persistent,
        }
        .capture(),
        (),
        test_options(),
    )?;
    assert_eq!(&*db.run(answer)?, "answer:one");
    let durable_bytes = db.statistics().stored_bytes;

    for (text, expected, executions) in [(" o n e ", "answer:one", 1), ("two", "answer:two", 2)] {
        db.replace_inputs(
            Source {
                text,
                lifetime: Lifetime::Session,
            }
            .capture(),
        );
        let large = db.run(|cx| {
            assert_eq!(&*answer(cx)?, expected);
            Ok(Arc::downgrade(&document(cx)?.2.value(cx)?))
        })?;
        assert_eq!(db.statistics().executed, executions);
        assert_eq!(db.statistics().bytes_written, 0);
        assert_eq!(db.statistics().stored_bytes, durable_bytes);
        assert!(
            db.statistics().session_bytes < 8192,
            "unchanged large payload stays on disk"
        );
        assert!(
            large.upgrade().is_none(),
            "the session does not retain an equal decoded output"
        );
        assert_eq!(&*db.run(answer)?, expected);
        assert_eq!(
            db.statistics().executed,
            0,
            "another request reuses the same revision"
        );
    }

    db.replace_inputs(
        Source {
            text: "two",
            lifetime: Lifetime::Persistent,
        }
        .capture(),
    );
    assert_eq!(&*db.run(answer)?, "answer:two");
    assert_eq!(db.statistics().executed, 0, "saving only changes retention");
    assert_eq!(db.statistics().session_queries, 0);
    drop(db);
    let mut db = Database::open(
        &path,
        "test",
        Source {
            text: "two",
            lifetime: Lifetime::Persistent,
        }
        .capture(),
        (),
        test_options(),
    )?;
    assert_eq!(&*db.run(answer)?, "answer:two");
    assert_eq!(
        db.statistics().executed,
        0,
        "promoted values survive restart"
    );
    Ok(())
}

#[test]
fn memory_session_results_survive_promotion_with_a_zero_disk_budget() -> Result<()> {
    let mut db = Database::memory(
        Source {
            text: "edited",
            lifetime: Lifetime::Session,
        }
        .capture(),
        (),
        DatabaseOptions::new(ajika::Retention {
            max_bytes: 0,
            ..retention()
        }),
    );
    assert_eq!(&*db.run(answer)?, "answer:edited");

    db.replace_inputs(
        Source {
            text: "edited",
            lifetime: Lifetime::Persistent,
        }
        .capture(),
    );
    assert_eq!(&*db.run(answer)?, "answer:edited");
    assert_eq!(
        db.statistics().executed,
        0,
        "saving keeps the computed answer"
    );
    assert_eq!(
        db.collect()?,
        0,
        "the disk budget does not evict typed values"
    );
    assert_eq!(&*db.run(answer)?, "answer:edited");
    assert_eq!(
        db.statistics().executed,
        0,
        "the promoted answer stays reusable"
    );
    Ok(())
}

#[test]
fn discarding_editor_work_recovers_the_saved_baseline() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(
        &path,
        "test",
        Source {
            text: "saved",
            lifetime: Lifetime::Persistent,
        }
        .capture(),
        (),
        test_options(),
    )?;
    db.run(answer)?;
    db.replace_inputs(
        Source {
            text: "edited",
            lifetime: Lifetime::Session,
        }
        .capture(),
    );
    assert_eq!(&*db.run(answer)?, "answer:edited");
    db.replace_inputs(
        Source {
            text: "saved",
            lifetime: Lifetime::Persistent,
        }
        .capture(),
    );
    db.clear_session();
    assert_eq!(&*db.run(answer)?, "answer:saved");
    assert_eq!(db.statistics().executed, 0);
    assert_eq!(db.statistics().session_bytes, 0);
    Ok(())
}

#[ajika::query]
fn page(db: &Db, key: u32) -> ajika::Result<String> {
    Ok(format!(
        "{key}:{}",
        Document::at(()).source(db)?.text.repeat(256)
    ))
}

#[test]
fn session_eviction_bounds_retained_work_and_recomputes_on_demand() -> Result<()> {
    for (case, max_entries, max_bytes, retained) in [
        ("entries", 2, u64::MAX, 2),
        ("bytes", usize::MAX, 2 * 1024, 1),
    ] {
        let source = Source {
            text: "edited",
            lifetime: Lifetime::Session,
        };
        let options = test_options().session_retention(ajika::Retention {
            max_entries,
            max_bytes,
            ..retention()
        });
        let mut db = Database::memory(source.capture(), (), options);
        // Request every key again so the assertion does not depend on which keys were evicted.
        for expected_executions in [3, 3 - retained] {
            db.run(|cx| {
                for key in 0..3 {
                    assert!(page(cx, key)?.starts_with(&format!("{key}:")));
                }
                Ok(())
            })?;
            let stats = db.statistics();
            assert_eq!(stats.executed, expected_executions, "{case} budget");
            assert_eq!(stats.session_queries, retained, "{case} budget");
            assert!(stats.session_bytes <= max_bytes, "{case} budget");
            assert_eq!(stats.stored_queries, 0);
        }
    }
    Ok(())
}

#[test]
fn session_reuses_equal_persistent_outputs_before_and_after_collect() -> Result<()> {
    let input = |text, lifetime| Source { text, lifetime }.capture();
    let options = test_options()
        .defer_checkpoints(1024 * 1024)
        .session_retention(ajika::Retention {
            max_bytes: 8 * 1024,
            ..retention()
        });
    let read = |db: &Db| {
        let large = document(db)?.2.value(db)?;
        assert_eq!(large.as_slice(), &[7; 64 * 1024]);
        assert_eq!(answer(db)?.as_str(), "answer:draft");
        Ok(Arc::downgrade(&large))
    };
    for collect_before_save in [false, true] {
        let directory = tempfile::tempdir()?;
        let mut db = Database::open(
            directory.path(),
            "test",
            input("saved", Lifetime::Persistent),
            (),
            options.clone(),
        )?;
        let saved = db.run(|cx| {
            answer(cx)?;
            Ok(Arc::downgrade(&document(cx)?.2.value(cx)?))
        })?;
        db.replace_inputs(input("draft", Lifetime::Session));
        let edited = db.run(read)?;
        assert!(
            edited.ptr_eq(&saved) || edited.upgrade().is_none(),
            "the session released its equal copy of the saved output"
        );
        db.run(read)?;
        assert_eq!(
            db.statistics().executed,
            0,
            "the edited result fits in 8 KiB"
        );
        assert_eq!(
            db.statistics().decoded,
            0,
            "the saved output is still in RAM"
        );
        let stats = db.statistics();
        assert_eq!(stats.metadata_reads + stats.header_reads, 0);
        assert!(stats.pending_queries > 0);

        if collect_before_save {
            assert_eq!(db.collect()?, 0);
            assert!(
                saved.upgrade().is_none(),
                "commit released the pending value"
            );
            db.run(read)?;
            let stats = db.statistics();
            assert_eq!(stats.executed, 0);
            assert!(stats.bytes_read >= 64 * 1024);
            assert_eq!(stats.metadata_reads + stats.header_reads, 0);
        }

        // Saving the exact edit must carry the large output forward, even if the
        // original pending write and the save coalesce into a single checkpoint.
        db.replace_inputs(input("draft", Lifetime::Persistent));
        db.run(answer)?;
        assert_eq!(db.statistics().executed, 0);
        db.collect()?;
        drop(db);
        let mut db = Database::open(
            directory.path(),
            "test",
            input("draft", Lifetime::Persistent),
            (),
            options.clone(),
        )?;
        db.run(read)?;
        assert_eq!(db.statistics().executed, 0);
    }
    Ok(())
}
