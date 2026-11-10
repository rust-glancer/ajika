use ajika::{Database, Inputs, Lifetime};
use divan::Bencher;

use super::support::{
    Fixture,
    workload::{Edit, File, Sources, View},
};

/// Change one file in the saved inputs and install the new snapshot.
/// Measures copying the file table, fingerprinting the edit, and detecting changes
/// as the number of saved files grows.
#[divan::bench(args = [32, 512, 8192], sample_count = 10, sample_size = 1)]
fn edit_saved_inputs(bencher: Bencher, files: u32) {
    let sources = Sources::new(files);
    let prepare = || Database::memory(sources.saved.clone(), (), Fixture::retention());
    let publish = |database: &mut Database| {
        database
            .update_inputs(|update| {
                File::at(0).set_source(update, sources.source(Edit::Meaning, Lifetime::Persistent))
            })
            .expect("publish saved edit");
    };
    {
        let mut database = prepare();
        publish(&mut database);
        let edited = database
            .inputs()
            .get(File::SOURCE, &0, &database)
            .expect("published source");
        assert_eq!(
            edited.text,
            sources.source(Edit::Meaning, Lifetime::Persistent).text
        );
        assert_eq!(edited.lifetime, Lifetime::Persistent);
    }
    bencher.with_inputs(prepare).bench_local_refs(publish);
}

/// Publish an unsaved edit using a sparse overlay of the saved inputs.
/// Measures changing one file while sharing the rest, as the saved project grows.
#[divan::bench(args = [32, 512, 8192], sample_count = 10, sample_size = 1)]
fn publish_editor_overlay(bencher: Bencher, files: u32) {
    let sources = Sources::new(files);
    let prepare = || Database::memory(sources.saved.clone(), (), Fixture::retention());
    let publish = |database: &mut Database| {
        database.replace_inputs(sources.edited(Edit::Meaning, View::Draft));
    };
    {
        let mut database = prepare();
        publish(&mut database);
        let draft = database
            .inputs()
            .get(File::SOURCE, &0, &database)
            .expect("draft source");
        assert_eq!(
            draft.text,
            sources.source(Edit::Meaning, Lifetime::Session).text
        );
        assert_eq!(draft.lifetime, Lifetime::Session);
        let saved = sources
            .saved
            .get(File::SOURCE, &0, &database)
            .expect("saved source");
        assert_eq!(
            saved.text,
            sources.source(Edit::Original, Lifetime::Persistent).text
        );
    }
    bencher.with_inputs(prepare).bench_local_refs(publish);
}

/// Capture a batch of file inputs, including computing their fingerprints.
/// Source text is prepared beforehand; this measures building the input tables.
#[divan::bench(args = [32, 512, 8192], sample_count = 10, sample_size = 1)]
fn capture_inputs(bencher: Bencher, files: u32) {
    let sources = Sources::new(files);
    let source = sources.source(Edit::Original, Lifetime::Persistent);
    let capture = || {
        Inputs::default()
            .edit(|update| {
                for file in 0..files {
                    File::at(file).set_source(update, source.clone())?;
                }
                Ok(())
            })
            .expect("capture input batch")
    };
    assert_eq!(capture().entries(File::SOURCE).len(), files as usize);
    bencher.bench_local(capture);
}
