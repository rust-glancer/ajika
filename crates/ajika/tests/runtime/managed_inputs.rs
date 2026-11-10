//! Host edits, query reads and restart validation use the same typed field declarations.

use std::{fs, sync::Arc};

use ajika::{Database, Durability, InputInit, Inputs, Interner, Lifetime, Stamp};
use anyhow::Result;

use super::retention;

type Db<'a> = ajika::Db<'a, ()>;

#[ajika::input(key = u32)]
struct Package {
    /// The source text is independent of the language edition.
    text: String,
    #[input(durability = High)]
    edition: u32,
    #[cfg_attr(all(), cfg(any()))]
    #[input(arc)]
    omitted: UnavailableType,
}

#[ajika::query]
fn edition(db: &Db) -> ajika::Result<u32> {
    Package::at(1).edition(db)
}

#[ajika::query]
fn text(db: &Db) -> ajika::Result<String> {
    Package::at(1).text(db)
}

#[test]
fn field_updates_are_private_until_published_and_saved_inputs_can_be_restored() -> Result<()> {
    let package = Package::at(1);
    let saved = Inputs::default().edit(|update| {
        package.set_text(update, "saved".into())?;
        package.set_edition(update, 2024)
    })?;
    let mut database = Database::memory(saved.clone(), (), retention());
    database.run(edition)?;

    let candidate = saved.edit(|update| package.set_text(update, "edited".into()))?;
    assert_eq!(&*database.run(text)?, "saved");
    database.replace_inputs(candidate);
    assert_eq!(*database.run(edition)?, 2024);
    assert_eq!(
        database.statistics().validated,
        0,
        "a text edit leaves edition-only work valid"
    );
    assert_eq!(&*database.run(text)?, "edited");

    database.replace_inputs(saved);
    assert_eq!(&*database.run(text)?, "saved");
    database.update_inputs(|update| package.set_text(update, "saved".into()))?;
    database.run(text)?;
    assert_eq!(
        database.statistics().validated,
        0,
        "equal assignments preserve validity"
    );

    let published = database.update_inputs(|update| {
        let text = String::from("published");
        package.set_text(update, text.clone())?;
        Ok(text)
    })?;
    assert_eq!(published, "published");
    assert_eq!(*database.run(text)?, published);
    Ok(())
}

#[test]
fn forgetting_an_input_clears_all_fields_for_its_key() -> Result<()> {
    let first = Package::at(1);
    let second = Package::at(2);
    let saved = Inputs::default().edit(|update| {
        first.set_text(update, "first".into())?;
        first.set_edition(update, 2024)?;
        second.set_text(update, "second".into())?;
        second.set_edition(update, 2021)
    })?;
    let edited = saved.overlay().edit(|update| {
        first.forget_all(update);
        Ok(())
    })?;
    assert_eq!(edited.entries(Package::TEXT), [(2, "second".to_owned())]);
    assert_eq!(edited.entries(Package::EDITION), [(2, 2021)]);
    Ok(())
}

#[ajika::interned]
struct FileId {
    path: String,
}

#[ajika::input(key = FileId)]
struct File {
    #[input(arc, initialize = File::load)]
    text: String,
}

impl File {
    fn load(id: &FileId, cx: &dyn InputInit) -> ajika::Result<String> {
        cx.check_cancelled()?;
        let path = id.path(&cx.interner())?;
        fs::read_to_string(path).map_err(ajika::Error::application)
    }
}

#[ajika::query]
fn length(db: &Db, id: FileId) -> ajika::Result<usize> {
    Ok(File::at(id).text(db)?.len())
}

#[test]
fn lazy_inputs_are_shared_with_the_host_and_recaptured_on_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory
        .path()
        .join("source")
        .to_str()
        .expect("temporary path")
        .to_owned();
    let storage = directory.path().join("queries");
    for (contents, expected_executions) in [("one", 1), ("one", 0), ("longer", 1)] {
        fs::write(&path, contents)?;
        let sources = Inputs::default();
        // The database adds configuration over these source tables. A hit must still capture
        // the source in those tables so the host can validate it before accepting its project.
        let mut database = Database::open(&storage, "inputs", sources.overlay(), (), retention())?;
        let id = FileId::new(&database.interner(), path.clone())?;
        let file = File::at(id);
        assert_eq!(*database.run(|db| length(db, id))?, contents.len());
        assert_eq!(database.statistics().executed, expected_executions);
        let captured = sources
            .peek(File::TEXT, &id)
            .expect("validation captured the input");
        database.run(|db| {
            let values = db.parallel_map(&[0, 1, 2, 3], |db, _| file.text(db))?;
            assert!(values.iter().all(|value| Arc::ptr_eq(value, &captured)));
            Ok(())
        })?;
    }
    Ok(())
}

#[test]
fn editor_overrides_leave_the_saved_values_and_other_files_shared() -> Result<()> {
    let interner = Interner::default();
    let a = FileId::new(&interner, "a".into())?;
    let b = FileId::new(&interner, "b".into())?;
    let shared = Arc::new("second".to_owned());
    let saved = Inputs::default().edit(|update| {
        File::at(a).set_text(update, "first".to_owned())?;
        File::at(b).set_text(update, shared.clone())
    })?;
    let editor = saved
        .overlay()
        .edit(|update| File::at(a).set_text(update, "edited".to_owned()))?;
    assert_eq!(&*saved.get(File::TEXT, &a, &interner)?, "first");
    assert_eq!(&*editor.get(File::TEXT, &a, &interner)?, "edited");
    assert!(Arc::ptr_eq(&shared, &saved.get(File::TEXT, &b, &interner)?));
    assert!(Arc::ptr_eq(
        &saved.get(File::TEXT, &b, &interner)?,
        &editor.get(File::TEXT, &b, &interner)?
    ));
    Ok(())
}

#[test]
fn forgetting_an_observation_recaptures_it_without_changing_the_saved_snapshot() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory
        .path()
        .join("source")
        .to_string_lossy()
        .into_owned();
    fs::write(&path, "before")?;
    let saved = Inputs::default();
    let database = Database::memory(saved.clone(), (), retention());
    let interner = database.interner();
    let id = FileId::new(&interner, path.clone())?;
    assert_eq!(&*saved.get(File::TEXT, &id, &database)?, "before");

    fs::write(&path, "after")?;
    let refreshed = saved.overlay().edit(|update| {
        update.forget(File::TEXT, id);
        Ok(())
    })?;
    assert_eq!(&*refreshed.get(File::TEXT, &id, &interner)?, "after");
    assert_eq!(&*saved.get(File::TEXT, &id, &database)?, "before");
    Ok(())
}

// The shared field supplies Clone through Arc; the payload needs neither Clone nor encoding
// when its stamp and policies are computed from borrowed observations.
struct SessionText(String);

impl SessionText {
    fn load(key: &u32, _cx: &dyn InputInit) -> ajika::Result<Arc<Self>> {
        Ok(Arc::new(Self(key.to_string())))
    }

    fn stamp(&self) -> ajika::Result<Stamp> {
        Ok(Stamp::of(self.0.as_bytes()))
    }

    fn durability(&self) -> Durability {
        Durability::High
    }

    fn lifetime(&self) -> Lifetime {
        Lifetime::Session
    }
}

#[ajika::input(
    key = u32,
    stamp = SessionText::stamp,
    durability = SessionText::durability,
    lifetime = SessionText::lifetime,
)]
struct SessionFile {
    #[input(arc, initialize = SessionText::load)]
    text: SessionText,
}

#[ajika::query]
fn session_length(db: &Db, key: u32) -> ajika::Result<usize> {
    Ok(SessionFile::at(key).text(db)?.0.len())
}

#[test]
fn arc_inputs_share_non_cloneable_payloads_and_apply_borrowed_policies() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let inputs = Inputs::default();
    let interner = Interner::default();
    let acquired = inputs.get(SessionFile::TEXT, &0, &interner)?;
    let shared = Arc::new(SessionText("shared".into()));
    let inputs = inputs.edit(|update| {
        SessionFile::at(1).set_text(update, SessionText("owned".into()))?;
        SessionFile::at(2).set_text(update, shared.clone())
    })?;
    assert!(Arc::ptr_eq(
        &shared,
        &inputs.get(SessionFile::TEXT, &2, &interner)?
    ));

    let mut database = Database::open(directory.path(), "inputs", inputs, (), retention())?;
    database.run(|db| {
        assert!(Arc::ptr_eq(&acquired, &SessionFile::at(0).text(db)?));
        for (key, expected) in [(0, 1), (1, 5), (2, 6)] {
            assert_eq!(*session_length(db, key)?, expected);
        }
        Ok(())
    })?;
    assert_eq!(database.statistics().executed, 3);
    assert_eq!(database.statistics().bytes_written, 0);
    assert_eq!(*database.run(|db| session_length(db, 2))?, 6);
    assert_eq!(database.statistics().executed, 0);
    Ok(())
}
