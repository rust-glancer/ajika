use divan::Bencher;

use super::support::{
    Backend, Case, Fixture,
    workload::{self, Edit, Project, View},
};

/// Build the whole index with no cached query results.
/// Includes recording dependencies and saving the completed work.
#[divan::bench(args = Case::ALL, sample_count = 10, sample_size = 1)]
fn cold_graph(bencher: Bencher, case: Case) {
    Fixture::bench(
        bencher,
        || Fixture::new(case),
        |fixture| fixture.database.run(workload::index),
        |fixture, answer| {
            assert_eq!(**answer, fixture.sources.expected_index(Edit::Original));
            assert_eq!(fixture.database.statistics().executed, case.query_count());
        },
    );
}

/// Read the same file summary 1,024 times in one operation.
/// The first read acquires its cached answer; the remaining reads share that value.
/// The caller is transient so its own answer does not need to be stored.
#[divan::bench(args = Case::ALL, sample_count = 10, sample_size = 1)]
fn resident_reads(bencher: Bencher, case: Case) {
    Fixture::bench(
        bencher,
        || Fixture::warm(case),
        |fixture| {
            fixture
                .database
                .run(|db| workload::repeated_reads(db, 1024))
        },
        |fixture, answer| {
            assert_eq!(**answer, 1024 * (16 * 7 + 2024));
            let stats = fixture.database.statistics();
            assert_eq!(stats.executed, 1);
            assert_eq!(stats.resident_hits, 1023);
        },
    );
}

/// Request the index in a new operation without changing any inputs.
/// Reuses the known-valid answer, reloading it from storage for a disk database.
#[divan::bench(args = Case::ALL, sample_count = 10, sample_size = 1)]
fn unchanged_request(bencher: Bencher, case: Case) {
    Fixture::bench(
        bencher,
        || Fixture::warm(case),
        |fixture| fixture.database.run(workload::index),
        |fixture, answer| {
            assert_eq!(**answer, fixture.sources.expected_index(Edit::Original));
            let stats = fixture.database.statistics();
            assert_eq!(stats.executed, 0);
            assert_eq!(stats.validated, 0);
            assert_eq!(
                stats.decoded,
                u64::from(matches!(case.backend, Backend::Disk))
            );
        },
    );
}

/// Request the index after an unrelated input changed.
/// Both have Low durability, so Ajika must check the saved dependencies before reusing
/// the answer. The input edit happens before timing.
#[divan::bench(args = Case::ALL, sample_count = 10, sample_size = 1)]
fn validate_unchanged_graph(bencher: Bencher, case: Case) {
    Fixture::bench(
        bencher,
        || {
            let mut fixture = Fixture::warm(case);
            fixture
                .database
                .update_inputs(|update| Project::at(()).set_unrelated_revision(update, 1))
                .expect("unrelated edit");
            fixture
        },
        |fixture| fixture.database.run(workload::index),
        |fixture, answer| {
            assert_eq!(**answer, fixture.sources.expected_index(Edit::Original));
            let stats = fixture.database.statistics();
            assert_eq!(stats.executed, 0);
            assert!(stats.validated > 0);
            assert_eq!(
                stats.decoded,
                u64::from(matches!(case.backend, Backend::Disk))
            );
        },
    );
}

/// Read cached configuration after an unrelated Low-durability input changed.
/// Configuration uses only High inputs, so it can skip checking dependencies.
#[divan::bench(args = Case::ALL, sample_count = 10, sample_size = 1)]
fn durability_shortcut(bencher: Bencher, case: Case) {
    Fixture::bench(
        bencher,
        || {
            let mut fixture = Fixture::warm(case);
            fixture
                .database
                .update_inputs(|update| Project::at(()).set_unrelated_revision(update, 1))
                .expect("low-durability edit");
            fixture
        },
        |fixture| fixture.database.run(workload::configuration),
        |fixture, answer| {
            assert_eq!(**answer, 2024);
            let stats = fixture.database.statistics();
            assert_eq!(stats.executed, 0);
            assert_eq!(stats.validated, 0);
        },
    );
}

/// Update the index after adding a name to one file.
/// Only that file's parse, analysis, and summary, plus the index, need to run.
/// The edited inputs are prepared before timing.
#[divan::bench(args = Case::ALL, sample_count = 10, sample_size = 1)]
fn meaningful_edit(bencher: Bencher, case: Case) {
    Fixture::bench(
        bencher,
        || {
            let mut fixture = Fixture::warm(case);
            fixture
                .database
                .replace_inputs(fixture.sources.edited(Edit::Meaning, View::Saved));
            fixture
        },
        |fixture| fixture.database.run(workload::index),
        |fixture, answer| {
            assert_eq!(**answer, fixture.sources.expected_index(Edit::Meaning));
            // Only one parse, its producer, its summary, and the aggregate execute.
            assert_eq!(fixture.database.statistics().executed, 4);
        },
    );
}

/// Check the index after adding whitespace to one file.
/// Parsing and analysis run, but unchanged names let summaries and the index keep
/// their cached answers. The whitespace edit happens before timing.
#[divan::bench(args = Case::ALL, sample_count = 10, sample_size = 1)]
fn equal_answer_edit(bencher: Bencher, case: Case) {
    Fixture::bench(
        bencher,
        || {
            let mut fixture = Fixture::warm(case);
            fixture
                .database
                .replace_inputs(fixture.sources.edited(Edit::Whitespace, View::Saved));
            fixture
        },
        |fixture| fixture.database.run(workload::index),
        |fixture, answer| {
            assert_eq!(**answer, fixture.sources.expected_index(Edit::Original));
            let stats = fixture.database.statistics();
            assert_eq!(
                stats.executed, 2,
                "parse and producer ran; names stayed equal"
            );
            assert!(stats.unchanged > 0);
            if matches!(case.backend, Backend::Disk) {
                assert!(stats.metadata_writes > 0);
            }
        },
    );
}
