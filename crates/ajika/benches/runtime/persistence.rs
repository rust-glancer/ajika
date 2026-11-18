use divan::{Bencher, black_box_drop};

use super::support::{
    Fixture, SavedCache,
    workload::{self, Edit},
};

/// Reopen a database that already contains a saved index.
#[divan::bench(args = [32], sample_count = 10, sample_size = 1)]
fn reopen(bencher: Bencher, files: u32) {
    let cache = SavedCache::new(files);
    // Returning the fixture lets Divan drop it after measurement, so redb's shutdown
    // commit does not obscure the cost of reopening a small cache.
    bencher.bench_local(|| cache.open());
}

/// Open and close a database that already contains a saved index.
/// Measures loading cache metadata without requesting any query values.
#[divan::bench(args = [512], sample_count = 10, sample_size = 1)]
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

/// Compare the response, immediately repeated response, and idle collection on the same
/// synthetic request. The graph contributes dependency metadata and the selected outputs
/// contribute about 16 MiB of payloads before compression.
mod write_behind {
    use divan::Bencher;

    use crate::support::{Case, Fixture, workload};

    #[derive(Clone, Copy, Debug)]
    enum Mode {
        Eager,
        Deferred,
    }

    impl Mode {
        fn fixture(self) -> Fixture {
            let options = Fixture::options();
            let options = match self {
                Self::Eager => options,
                Self::Deferred => options.defer_checkpoints(64 * 1024 * 1024),
            };
            Fixture::with_options(Case::disk(512), options)
        }
    }

    fn request(fixture: &mut Fixture) -> ajika::Result<u64> {
        fixture.database.run(|db| {
            let mut total = *workload::index(db)?;
            for bytes in 1_048_576..1_048_592 {
                total += workload::payloads(db, bytes)?.1.value(db)?.len() as u64;
            }
            Ok(total)
        })
    }

    #[divan::bench(args = [Mode::Eager, Mode::Deferred], sample_count = 10, sample_size = 1)]
    fn first(bencher: Bencher, mode: Mode) {
        Fixture::bench(
            bencher,
            || mode.fixture(),
            request,
            |fixture, value| {
                let index = fixture.sources.expected_index(workload::Edit::Original);
                assert_eq!(*value, index + (1_048_576..1_048_592u64).sum::<u64>());
            },
        );
    }

    #[divan::bench(args = [Mode::Eager, Mode::Deferred], sample_count = 10, sample_size = 1)]
    fn second(bencher: Bencher, mode: Mode) {
        Fixture::bench(
            bencher,
            || {
                let mut fixture = mode.fixture();
                request(&mut fixture).expect("first request");
                fixture
            },
            request,
            |fixture, _| assert_eq!(fixture.database.statistics().executed, 0),
        );
    }

    #[divan::bench(args = [Mode::Eager, Mode::Deferred], sample_count = 10, sample_size = 1)]
    fn collect(bencher: Bencher, mode: Mode) {
        Fixture::bench(
            bencher,
            || {
                let mut fixture = mode.fixture();
                request(&mut fixture).expect("first request");
                request(&mut fixture).expect("second request");
                fixture
            },
            |fixture| fixture.database.collect(),
            |fixture, _| {
                assert_eq!(
                    fixture.database.statistics().stored_queries,
                    Case::disk(512).query_count() + 16
                );
            },
        );
    }
}
