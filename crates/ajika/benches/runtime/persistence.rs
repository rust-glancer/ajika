use divan::{Bencher, black_box_drop};

use super::support::{
    Fixture, SavedCache,
    workload::{self, Edit},
};

/// Open and close a database that already contains a saved index.
/// Measures loading cache metadata without requesting any query values.
#[divan::bench(args = [32, 512], sample_count = 10, sample_size = 1)]
fn open_and_close(bencher: Bencher, files: u32) {
    let cache = SavedCache::new(files);
    bencher.bench_local(|| black_box_drop(cache.open()));
}

/// Request the index immediately after reopening a populated cache.
/// Checks saved dependencies and decodes only the index; temporary parse results
/// do not need recreating. Opening the database happens before timing.
#[divan::bench(args = [32, 512], sample_count = 10, sample_size = 1)]
fn first_request(bencher: Bencher, files: u32) {
    let cache = SavedCache::new(files);
    Fixture::bench(
        bencher,
        || cache.open(),
        |fixture| fixture.database.run(workload::index),
        |fixture, answer| {
            assert_eq!(**answer, fixture.sources.expected_index(Edit::Original));
            let stats = fixture.database.statistics();
            assert_eq!(
                stats.executed, 0,
                "transient parses need not be reconstructed"
            );
            assert!(stats.validated > 0);
            assert_eq!(stats.decoded, 1, "only the root payload is requested");
        },
    );
}

/// Request a parse result after reopening the cache.
/// Its dependencies were saved but its value was not, so parsing runs again.
/// Opening the database happens before timing.
#[divan::bench(args = [32, 512], sample_count = 10, sample_size = 1)]
fn reconstruct_transient(bencher: Bencher, files: u32) {
    let cache = SavedCache::new(files);
    Fixture::bench(
        bencher,
        || cache.open(),
        |fixture| fixture.database.run(|db| workload::parse(db, 0)),
        |fixture, _| {
            let stats = fixture.database.statistics();
            assert_eq!(stats.executed, 1);
            assert_eq!(stats.decoded, 0);
            assert_eq!(stats.bytes_written, 0);
        },
    );
}
