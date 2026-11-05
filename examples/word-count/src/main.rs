// ANCHOR: queries
use std::{sync::Arc, time::Duration};

use ajika::{Database, Db, Inputs, Result, Retention};

// ANCHOR: declarations
#[ajika::input(key = String)]
struct File {
    #[input(arc)]
    text: String,
}

#[ajika::query]
fn words(db: &Db<'_>, path: String) -> Result<Vec<String>> {
    let text = File::at(path).text(db)?;
    Ok(text.split_whitespace().map(str::to_owned).collect())
}

#[ajika::query]
fn word_count(db: &Db<'_>, path: String) -> Result<usize> {
    Ok(words(db, path)?.len())
}
// ANCHOR_END: declarations
// ANCHOR_END: queries

// ANCHOR: forward
// This convenience query reuses the words query's allocation.
#[ajika::query(forward)]
fn note_words(db: &Db<'_>) -> Result<Arc<Vec<String>>> {
    words(db, "notes.txt".into())
}
// ANCHOR_END: forward

fn main() -> Result<()> {
    // ANCHOR: run
    let limits = Retention {
        max_entries: 10_000,
        max_bytes: 256 * 1024 * 1024,
        max_idle: Duration::from_secs(24 * 60 * 60),
    };
    let mut database = Database::memory(Inputs::default(), (), limits);
    let file = File::at("notes.txt".into());
    database.update_inputs(|update| file.set_text(update, "hello world".to_owned()))?;

    let count = database.run(|db| word_count(db, "notes.txt".into()))?;
    assert_eq!(*count, 2);
    assert_eq!(database.statistics().executed, 2);
    // ANCHOR_END: run

    // ANCHOR: unchanged
    database.run(|db| word_count(db, "notes.txt".into()))?;
    assert_eq!(database.statistics().executed, 0);

    database.update_inputs(|update| file.set_text(update, "hello    world".to_owned()))?;
    database.run(|db| word_count(db, "notes.txt".into()))?;
    assert_eq!(database.statistics().executed, 1); // Only words ran.
    // ANCHOR_END: unchanged

    // ANCHOR: changed
    database.update_inputs(|update| file.set_text(update, "hello new world".to_owned()))?;
    let count = database.run(|db| word_count(db, "notes.txt".into()))?;
    assert_eq!(*count, 3);
    assert_eq!(database.statistics().executed, 2);
    // ANCHOR_END: changed
    database.run(|db| {
        let words = words(db, "notes.txt".into())?;
        assert!(Arc::ptr_eq(&words, &note_words(db)?));
        Ok(())
    })?;
    Ok(())
}
