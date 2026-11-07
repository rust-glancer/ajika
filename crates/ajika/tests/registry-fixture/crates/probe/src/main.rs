use std::{env, sync::Arc, time::Duration};

use ajika::{__private::Registration, Database, Retention};
use fixture_queries::{File, FileId};

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args_os().collect();
    let step: usize = args[1].to_str().expect("numeric step").parse()?;
    let (source, expected, executed) = [
        ("one two", "Words: 2", 2),
        ("one  two", "Words: 2", 1),
        ("one two three", "Words: 3", 2),
    ][step];

    // This fixture specifically checks linker retention, including the uncalled declaration.
    assert_eq!(
        ajika::__private::inventory::iter::<Registration>
            .into_iter()
            .count(),
        6
    );
    let limits = Retention {
        max_entries: 100,
        max_bytes: 1024 * 1024,
        max_idle: Duration::from_secs(3600),
    };
    let inputs = ajika::Inputs::default().edit(|update| {
        for file in [0, 1] {
            File::at(file).set_text(update, Arc::new(source.to_owned()))?;
        }
        Ok(())
    })?;
    let mut database = Database::open(&args[2], "registry-fixture", inputs, (), limits)?;
    // The declaring crate needs only Ajika, and consumers use the generated public API.
    let interner = database.interner();
    let file = FileId::new(&interner, "src/lib.rs".into())?;
    assert_eq!(file.path(&interner)?, "src/lib.rs");
    assert_eq!(file.as_raw(), 0);
    let value = database.run(|db| fixture_queries::summary(db, 0, "Words".into()))?;
    assert_eq!(database.statistics().executed, executed);
    assert_eq!(value.as_str(), expected);

    database.run(|db| {
        let value = fixture_queries::summary(db, 0, "Words".into())?;
        assert!(Arc::ptr_eq(
            &value,
            &fixture_queries::summary(db, 0, "Words".into())?
        ));
        Ok(())
    })?;
    assert_eq!(database.statistics().executed, 0);

    // Whitespace changes keep the separately stored outputs equal. Only the parallel child
    // should execute; without its linked registration, the parent would execute as well.
    let parallel = database.run(|db| fixture_queries::parallel_summary(db, 0))?;
    assert_eq!(database.statistics().executed, executed);
    assert_eq!(parallel.as_str(), expected);

    database.run(|db| {
        let outputs = db.parallel_map(&[0, 1], |db, file| fixture_queries::lengths(db, *file))?;
        assert_eq!(*outputs[0][0].value(db)?, 3);
        let words = fixture_queries::WordOutputs::for_file(db, 1)?;
        assert_eq!(
            *words.count.value(db)?,
            source.split_whitespace().count() as u64
        );
        assert_eq!(
            *words.bytes.as_ref().expect("source bytes").value(db)?,
            source.len() as u64
        );
        Ok(())
    })?;
    Ok(())
}
