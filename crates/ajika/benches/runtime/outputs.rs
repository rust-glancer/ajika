use divan::Bencher;

use super::support::{
    Case, Fixture,
    workload::{self, Edit, View},
};

/// Load a small output whose producer also has a much larger output.
/// Uses an existing handle, so neither the producer's return value nor its large
/// output should need decoding.
#[divan::bench(args = [512, 64 * 1024, 1024 * 1024], sample_count = 10, sample_size = 1)]
fn selected_small_output(bencher: Bencher, bytes: u32) {
    let mut fixture = Fixture::new(Case::disk(1));
    let outputs = fixture
        .database
        .run(|db| workload::payloads(db, bytes))
        .expect("populate payloads");
    let read = |fixture: &mut Fixture| fixture.database.run(|db| outputs.0.value(db));
    assert_eq!(*read(&mut fixture).expect("read small output"), 42);
    assert_eq!(fixture.database.statistics().decoded, 1);
    assert_eq!(fixture.database.statistics().executed, 0);
    bencher.bench_local(|| read(&mut fixture).expect("read small output"));
}

/// Load and decode one stored output at different sizes.
/// Each read starts a new operation, so the decoded value must be loaded again.
#[divan::bench(args = [512, 64 * 1024, 1024 * 1024], sample_count = 10, sample_size = 1)]
fn selected_large_output(bencher: Bencher, bytes: u32) {
    let mut fixture = Fixture::new(Case::disk(1));
    let outputs = fixture
        .database
        .run(|db| workload::payloads(db, bytes))
        .expect("populate payloads");
    let read = |fixture: &mut Fixture| fixture.database.run(|db| outputs.1.value(db));
    assert_eq!(
        read(&mut fixture).expect("read large output").len(),
        bytes as usize
    );
    assert_eq!(fixture.database.statistics().decoded, 1);
    assert_eq!(fixture.database.statistics().executed, 0);
    bencher.bench_local(|| read(&mut fixture).expect("read large output"));
}

/// Request the index and navigation after whitespace shifts the names in one file.
/// Locations change but names do not, so navigation recomputes while the index
/// reuses its answer. The edit happens before timing.
#[divan::bench(args = Case::ALL, sample_count = 10, sample_size = 1)]
fn change_one_output(bencher: Bencher, case: Case) {
    Fixture::bench(
        bencher,
        || {
            let mut fixture = Fixture::warm(case);
            fixture
                .database
                .run(|db| workload::navigation(db, 0))
                .expect("populate navigation");
            fixture
                .database
                .replace_inputs(fixture.sources.edited(Edit::Whitespace, View::Saved));
            fixture
        },
        |fixture| {
            fixture.database.run(|db| {
                workload::index(db)?;
                workload::navigation(db, 0)
            })
        },
        |fixture, answer| {
            // Original token offsets are 0, 8, ..., 120. One leading space shifts all 16.
            assert_eq!(**answer, 16 * 120 / 2 + 16);
            assert_eq!(
                fixture.database.statistics().executed,
                3,
                "parse, producer, and navigation ran; names consumers stayed green"
            );
        },
    );
}
