// ANCHOR: input
use std::{sync::Arc, time::Duration};

use ajika::{Database, Db, Inputs, Lifetime, Retention, Stamp};

#[derive(Clone)]
struct Text {
    contents: Arc<String>,
    lifetime: Lifetime,
}

impl Text {
    fn stamp(&self) -> ajika::Result<Stamp> {
        Ok(Stamp::of(self.contents.as_bytes()))
    }

    fn lifetime(&self) -> Lifetime {
        self.lifetime
    }
}

#[ajika::input(key = String)]
struct File {
    #[input(stamp = Text::stamp, lifetime = Text::lifetime)]
    text: Text,
}
// ANCHOR_END: input

#[ajika::query]
fn word_count(db: &Db<'_>, path: String) -> ajika::Result<usize> {
    Ok(File::at(path).text(db)?.contents.split_whitespace().count())
}

fn main() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut database = Database::open(
        directory.path().join("queries"),
        "editor-v1",
        Inputs::default(),
        (),
        Retention {
            max_entries: 10_000,
            max_bytes: 256 * 1024 * 1024,
            max_idle: Duration::from_secs(24 * 60 * 60),
        },
    )?;
    let file = File::at("notes.txt".into());
    database.update_inputs(|update| {
        file.set_text(
            update,
            Text {
                contents: Arc::new("hello world".into()),
                lifetime: Lifetime::Persistent,
            },
        )
    })?;
    database.run(|db| word_count(db, "notes.txt".into()))?;

    // ANCHOR: overlay
    let saved = database.inputs().clone();
    let edited = Arc::new("hello new world".to_owned());
    let editor = saved.overlay().edit(|update| {
        file.set_text(
            update,
            Text {
                contents: edited.clone(),
                lifetime: Lifetime::Session,
            },
        )
    })?;
    database.replace_inputs(editor);
    assert_eq!(*database.run(|db| word_count(db, "notes.txt".into()))?, 3);
    assert_eq!(database.statistics().bytes_written, 0);

    database.run(|db| word_count(db, "notes.txt".into()))?;
    assert_eq!(database.statistics().executed, 0);
    assert_eq!(database.statistics().decoded, 0);
    // ANCHOR_END: overlay

    // ANCHOR: save
    // The host has saved these exact bytes successfully.
    let saved = saved.edit(|update| {
        file.set_text(
            update,
            Text {
                contents: edited,
                lifetime: Lifetime::Persistent,
            },
        )
    })?;
    database.replace_inputs(saved);
    database.run(|db| word_count(db, "notes.txt".into()))?;
    assert_eq!(database.statistics().executed, 0);
    assert_eq!(database.statistics().session_queries, 0);
    // ANCHOR_END: save
    Ok(())
}
