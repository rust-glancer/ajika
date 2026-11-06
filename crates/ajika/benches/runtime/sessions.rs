use divan::Bencher;

use super::support::{
    Case, Fixture,
    workload::{self, Edit, View},
};

/// Update the index for an unsaved edit to one file.
/// Changed answers stay in session memory while the saved disk results remain intact.
/// The edited inputs are prepared before timing.
#[divan::bench(args = [32, 512], sample_count = 10, sample_size = 1)]
fn draft_edit(bencher: Bencher, files: u32) {
    Fixture::bench(
        bencher,
        || {
            let mut fixture = Fixture::warm(Case::disk(files));
            fixture
                .database
                .replace_inputs(fixture.sources.edited(Edit::Meaning, View::Draft));
            fixture
        },
        |fixture| fixture.database.run(workload::index),
        |fixture, answer| {
            assert_eq!(**answer, fixture.sources.expected_index(Edit::Meaning));
            let stats = fixture.database.statistics();
            assert_eq!(stats.executed, 4);
            assert_eq!(stats.bytes_written, 0);
            assert_eq!(stats.metadata_writes, 0);
            assert!(stats.session_queries > 0);
        },
    );
}

/// Request the index again while the unsaved inputs are unchanged.
/// Measures reuse from session memory, without query execution or disk decoding.
#[divan::bench(args = [32, 512], sample_count = 10, sample_size = 1)]
fn repeated_draft_request(bencher: Bencher, files: u32) {
    Fixture::bench(
        bencher,
        || {
            let mut fixture = Fixture::warm(Case::disk(files));
            fixture
                .database
                .replace_inputs(fixture.sources.edited(Edit::Meaning, View::Draft));
            fixture
                .database
                .run(workload::index)
                .expect("populate draft");
            fixture
        },
        |fixture| fixture.database.run(workload::index),
        |fixture, answer| {
            assert_eq!(**answer, fixture.sources.expected_index(Edit::Meaning));
            let stats = fixture.database.statistics();
            assert_eq!(stats.executed, 0);
            assert_eq!(stats.decoded, 0);
            assert_eq!(stats.bytes_written, 0);
        },
    );
}

/// Persist answers already computed for an unsaved edit.
/// The saved text matches the draft, so Ajika can move its results to disk without
/// rerunning queries. The new input snapshot is installed before timing.
#[divan::bench(args = [32, 512], sample_count = 10, sample_size = 1)]
fn promote_on_save(bencher: Bencher, files: u32) {
    Fixture::bench(
        bencher,
        || {
            let mut fixture = Fixture::warm(Case::disk(files));
            fixture
                .database
                .replace_inputs(fixture.sources.edited(Edit::Meaning, View::Draft));
            fixture
                .database
                .run(workload::index)
                .expect("populate draft");
            fixture
                .database
                .replace_inputs(fixture.sources.edited(Edit::Meaning, View::Saved));
            fixture
        },
        |fixture| fixture.database.run(workload::index),
        |fixture, answer| {
            assert_eq!(**answer, fixture.sources.expected_index(Edit::Meaning));
            let stats = fixture.database.statistics();
            assert_eq!(stats.executed, 0, "saving only changes retention");
            assert_eq!(stats.session_queries, 0);
            assert!(stats.bytes_written > 0);
        },
    );
}
