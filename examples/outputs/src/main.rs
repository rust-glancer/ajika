use std::time::Duration;

use ajika::{Database, Db, Inputs, Result, Retention};

#[ajika::input(key = String)]
struct File {
    #[input(arc)]
    text: String,
}

// ANCHOR: producer
#[ajika::output]
struct FileOutputs {
    words: Vec<String>,
    locations: Vec<(usize, usize)>,
}

#[ajika::query]
fn analyze(db: &Db<'_>, path: String) -> Result<FileOutputs> {
    let text = File::at(path).text(db)?;
    let mut words = Vec::new();
    let mut locations = Vec::new();
    let mut cursor = 0;
    for word in text.split_whitespace() {
        let start = cursor + text[cursor..].find(word).expect("word comes from text");
        let end = start + word.len();
        words.push(word.to_owned());
        locations.push((start, end));
        cursor = end;
    }
    FileOutputs::new(db, words, locations)
}
// ANCHOR_END: producer

// ANCHOR: consumer
#[ajika::query]
fn word_count(db: &Db<'_>, path: String) -> Result<usize> {
    let outputs = analyze(db, path)?;
    let words = outputs.words.value(db)?;
    Ok(words.len())
}
// ANCHOR_END: consumer

fn main() -> Result<()> {
    let mut database = Database::memory(
        Inputs::default(),
        (),
        Retention {
            max_entries: 10_000,
            max_bytes: 256 * 1024 * 1024,
            max_idle: Duration::from_secs(24 * 60 * 60),
        },
    );
    // ANCHOR: edit
    let file = File::at("notes.txt".into());
    database.update_inputs(|update| file.set_text(update, "hello world".to_owned()))?;
    database.run(|db| word_count(db, "notes.txt".into()))?;
    let locations = database.run(|db| Ok(analyze(db, "notes.txt".into())?.locations.clone()))?;

    database.update_inputs(|update| file.set_text(update, "hello  world".to_owned()))?;
    assert_eq!(*database.run(|db| word_count(db, "notes.txt".into()))?, 2);
    assert_eq!(database.statistics().executed, 1); // analyze ran; word_count stayed valid.
    assert_eq!(
        *database.run(|db| locations.value(db))?,
        vec![(0, 5), (7, 12)]
    );
    // ANCHOR_END: edit
    Ok(())
}
