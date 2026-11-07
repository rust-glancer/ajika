use std::sync::Arc;

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

#[ajika::query]
fn packed_value(_cx: &Db, byte: u8) -> ajika::Result<Vec<u8>> {
    Ok(vec![byte; 4096])
}

fn truncate_only_pack(root: &std::path::Path) -> Result<()> {
    let pack = std::fs::read_dir(root.join("packs"))?
        .next()
        .expect("one packed value")?
        .path();
    // Keep the pack header and remove the frames so a live read encounters the damage.
    std::fs::OpenOptions::new()
        .write(true)
        .open(pack)?
        .set_len(40)?;
    Ok(())
}

struct DamagedCache {
    root: tempfile::TempDir,
    db: Database,
    saved: Arc<File>,
}

impl DamagedCache {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let mut db = Database::open(root.path(), "test", inputs(""), (), retention())?;
        db.set_compression(ajika::Compression::None);
        let saved = db.run(|cx| {
            packed_value(cx, 42)?;
            file_id(cx, "saved.rs".into())
        })?;
        truncate_only_pack(root.path())?;
        Ok(Self { root, db, saved })
    }
}

#[test]
fn restart_repairs_a_damaged_pack_and_keeps_saved_ids() -> Result<()> {
    let DamagedCache { root, db, saved } = DamagedCache::new()?;
    drop(db);

    let mut db = Database::open(root.path(), "test", inputs(""), (), retention())?;
    assert_eq!(saved.path(&db.interner())?, "saved.rs");
    assert_eq!(db.run(|cx| packed_value(cx, 42))?.as_slice(), &[42; 4096]);
    assert_eq!(db.statistics().executed, 1);
    Ok(())
}

#[test]
fn next_run_repairs_the_cache_and_preserves_live_ids() -> Result<()> {
    let DamagedCache {
        root,
        mut db,
        saved,
    } = DamagedCache::new()?;
    let interner = db.interner();
    let pending = File::new(&interner, "pending.rs".into())?;
    let mut calls = 0;
    db.run(|cx| {
        calls += 1;
        packed_value(cx, 42)
    })
    .expect_err("the original request sees the damaged value");
    assert_eq!(calls, 1);
    assert!(db.recovery_pending());

    assert_eq!(db.run(|cx| packed_value(cx, 42))?.as_slice(), &[42; 4096]);
    assert_eq!(db.statistics().executed, 1);
    assert!(!db.recovery_pending());
    assert_eq!(saved.path(&db.interner())?, "saved.rs");
    assert_eq!(pending.path(&db.interner())?, "pending.rs");
    assert_eq!(pending.path(&interner)?, "pending.rs");

    drop(db);
    let db = Database::open(root.path(), "test", inputs(""), (), retention())?;
    assert_eq!(pending.path(&db.interner())?, "pending.rs");
    Ok(())
}

#[test]
fn invalidation_repairs_the_cache_and_persists_live_ids() -> Result<()> {
    let DamagedCache {
        root,
        mut db,
        saved,
    } = DamagedCache::new()?;
    let interner = db.interner();
    let pending = File::new(&interner, "pending.rs".into())?;
    db.run(|cx| packed_value(cx, 42))
        .expect_err("the damaged value schedules repair");
    assert!(db.recovery_pending());

    db.invalidate()?;
    assert!(!db.recovery_pending());
    assert_eq!(pending.path(&db.interner())?, "pending.rs");
    assert_eq!(pending.path(&interner)?, "pending.rs");
    drop(db);

    // Reopen before any query checkpoint can save the pending assignment.
    let mut db = Database::open(root.path(), "test", inputs(""), (), retention())?;
    assert_eq!(pending.path(&db.interner())?, "pending.rs");
    assert_eq!(saved.path(&db.interner())?, "saved.rs");
    assert_eq!(db.run(|cx| packed_value(cx, 42))?.as_slice(), &[42; 4096]);
    assert_eq!(db.statistics().executed, 1);
    Ok(())
}

#[test]
fn successful_run_can_leave_recovery_pending() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut db = Database::open(root.path(), "test", inputs(""), (), retention())?;
    db.run(|cx| packed_value(cx, 0))?;
    truncate_only_pack(root.path())?;

    // Independent writes let maintenance find the damage when it consolidates packs.
    for byte in 1..=32 {
        assert_eq!(db.run(|cx| packed_value(cx, byte))?[0], byte);
        if db.recovery_pending() {
            break;
        }
    }
    assert!(db.recovery_pending(), "maintenance found the damaged pack");

    db.cancellation().cancel();
    let error = db.run(|_| Ok(())).expect_err("request was cancelled");
    assert!(error.is_cancelled());
    assert!(db.recovery_pending());

    db.run(|_| Ok(()))?;
    assert!(!db.recovery_pending());
    Ok(())
}

#[test]
fn an_aborted_pack_write_keeps_the_warm_checkpoint() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut db = Database::open(root.path(), "test", inputs(""), (), retention())?;
    let saved = db.run(|cx| file_id(cx, "saved.rs".into()))?;
    std::fs::remove_dir(root.path().join("packs"))?;
    db.run(|cx| packed_value(cx, 42))
        .expect_err("a pack needs a writable directory");
    assert!(!db.recovery_pending());
    std::fs::create_dir(root.path().join("packs"))?;

    assert_eq!(db.run(|cx| file_id(cx, "saved.rs".into()))?, saved);
    assert_eq!(db.statistics().executed, 0, "the saved answer stayed warm");
    assert_eq!(db.run(|cx| packed_value(cx, 42))?.as_slice(), &[42; 4096]);
    Ok(())
}
