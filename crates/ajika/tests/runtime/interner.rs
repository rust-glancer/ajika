use ajika as renamed;
use ajika::Database;
use anyhow::Result;

use super::{Db, inputs, retention};

#[ajika::interned(namespace = "files")]
struct File {
    path: String,
}

#[ajika::query]
fn file_id(cx: &Db, path: String) -> ajika::Result<File> {
    File::new(&cx.interner(), path)
}

#[test]
fn interned_ids_survive_parallel_discovery_collection_and_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(&path, "test", inputs(""), (), retention())?;
    let ids = db.run(|cx| {
        cx.parallel_map(&["a.rs", "b.rs", "a.rs"], |cx, path| {
            file_id(cx, (*path).into())
        })
    })?;
    assert_eq!(ids[0], ids[2]);
    assert_ne!(ids[0], ids[1]);

    // Collection preserves saved assignments, pending assignments and keys held by callers.
    let interner = db.interner();
    let first_key = ids[0].path(&interner)?;
    let pending = File::new(&interner, "c.rs".into())?;
    db.collect()?;
    assert_eq!(first_key, "a.rs");
    assert_eq!(ids[0].path(&interner)?, "a.rs");

    // Query collection cannot retire IDs that the host or another saved result may still use.
    db.invalidate()?;
    for (name, expected) in [("b.rs", ids[1].clone()), ("a.rs", ids[0].clone())] {
        assert_eq!(db.run(|cx| file_id(cx, name.into()))?, expected);
    }
    drop(db);
    let mut db = Database::open(path, "test", inputs(""), (), retention())?;
    let interner = db.interner();
    assert_eq!(pending.path(&interner)?, "c.rs");
    assert_eq!(File::new(&interner, "b.rs".into())?, *ids[1]);
    assert_eq!(File::new(&interner, "a.rs".into())?, *ids[0]);
    assert_eq!(File::new(&interner, "c.rs".into())?, pending);
    assert_eq!(db.run(|cx| file_id(cx, "a.rs".into()))?, ids[0]);
    assert_eq!(db.statistics().executed, 0);
    Ok(())
}

#[test]
fn interning_does_not_write_without_a_durable_checkpoint() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("queries");
    let mut db = Database::open(&path, "test", inputs(""), (), retention())?;
    db.run(|cx| {
        File::new(&cx.interner(), "editor.rs".into())?;
        Ok(())
    })?;
    assert_eq!(db.statistics().bytes_written, 0);
    drop(db);
    // No stored value could refer to editor.rs, so it did not acquire durable identity yet.
    let db = Database::open(path, "test", inputs(""), (), retention())?;
    assert_eq!(File::new(&db.interner(), "saved.rs".into())?.as_raw(), 0);
    Ok(())
}

#[renamed::interned(crate = renamed)]
struct Symbol {
    text: String,
    disambiguator: u32,
    #[cfg(any())]
    disabled: UnknownType,
}

#[test]
fn declarations_have_independent_numbering() -> Result<()> {
    let interner = ajika::Interner::default();
    let first = File::new(&interner, "main.rs".into())?;
    let second = File::new(&interner, "other.rs".into())?;
    assert_ne!(first, second);
    assert_eq!(first.path(&interner)?, "main.rs");
    assert_eq!(second.path(&interner)?, "other.rs");
    assert_eq!(File::new(&interner.clone(), "main.rs".into())?, first);

    // The complete set of fields determines identity, independently of the file declaration.
    let symbol = Symbol::new(&interner, "main".into(), 1)?;
    assert_eq!(symbol.as_raw(), first.as_raw());
    assert_eq!(symbol, Symbol::new(&interner, "main".into(), 1)?);
    assert_ne!(symbol, Symbol::new(&interner, "main".into(), 2)?);
    assert_eq!(symbol.text(&interner)?, "main");
    assert_eq!(symbol.disambiguator(&interner)?, 1);
    assert_eq!(Symbol::from_raw(symbol.as_raw()), symbol);
    Ok(())
}
