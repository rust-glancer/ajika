use std::time::Duration;

use ajika::{Database, Db, Inputs, Result, Retention};

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

fn main() -> Result<()> {
    // ANCHOR: first-open
    // A real application chooses a persistent cache directory.
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let limits = Retention {
        max_entries: 10_000,
        max_bytes: 256 * 1024 * 1024,
        max_idle: Duration::from_secs(24 * 60 * 60),
    };

    let mut database = Database::open(&path, "word-count-v1", Inputs::default(), (), limits)?;
    database.update_inputs(|update| {
        File::at("notes.txt".into()).set_text(update, "hello world".to_owned())
    })?;
    assert_eq!(*database.run(|db| word_count(db, "notes.txt".into()))?, 2);
    assert_eq!(database.statistics().executed, 2);
    drop(database);
    // ANCHOR_END: first-open

    // ANCHOR: reopen
    // Simulate fresh acquisition by the application, without reusing the old Inputs.
    let inputs = Inputs::default()
        .edit(|update| File::at("notes.txt".into()).set_text(update, "hello world".to_owned()))?;
    let mut database = Database::open(&path, "word-count-v1", inputs, (), limits)?;
    assert_eq!(*database.run(|db| word_count(db, "notes.txt".into()))?, 2);
    assert_eq!(database.statistics().executed, 0);
    assert_eq!(database.statistics().validated, 2);
    // ANCHOR_END: reopen

    // ANCHOR: later-operation
    database.run(|db| word_count(db, "notes.txt".into()))?;
    assert_eq!(database.statistics().executed, 0);
    assert_eq!(database.statistics().validated, 0);
    assert_eq!(database.statistics().decoded, 1);
    // ANCHOR_END: later-operation
    Ok(())
}
