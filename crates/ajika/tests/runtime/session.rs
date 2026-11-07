use std::sync::Arc;

use ajika::{Database, Inputs, Lifetime, Output, Stamp};
use anyhow::Result;

use super::retention;

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
        retention(),
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
        retention(),
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
        ajika::Retention {
            max_bytes: 0,
            ..retention()
        },
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
        retention(),
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
    for disk in [false, true] {
        let directory = tempfile::tempdir()?;
        let source = Source {
            text: "edited",
            lifetime: Lifetime::Session,
        };
        let mut db = if disk {
            Database::open(
                directory.path().join("queries"),
                "test",
                source.capture(),
                (),
                retention(),
            )?
        } else {
            Database::memory(source.capture(), (), retention())
        };
        db.set_session_retention(ajika::Retention {
            max_entries: 2,
            max_bytes: 4096,
            ..retention()
        });
        for key in 0..20 {
            assert!(db.run(|cx| page(cx, key))?.starts_with(&format!("{key}:")));
            assert!(db.statistics().session_queries <= 2);
            assert!(db.statistics().session_bytes <= 4096);
            assert_eq!(db.statistics().stored_queries, 0);
        }
        assert!(db.run(|cx| page(cx, 0))?.starts_with("0:"));
        assert_eq!(db.statistics().executed, 1);
    }
    Ok(())
}
