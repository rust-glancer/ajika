use divan::Bencher;

use super::support::Fixture;

/// Run one bounded cleanup pass after half of a pack's values have been replaced.
/// Measures copying its remaining live values and removing the obsolete pack.
/// Preparing the fragmented store happens before timing.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn bounded_maintenance(bencher: Bencher) {
    Fixture::bench(
        bencher,
        Fixture::fragmented,
        |fixture| fixture.database.maintain(),
        |fixture, _| {
            let stats = fixture
                .database
                .statistics()
                .expect("maintained statistics");
            assert_eq!(stats.dead_pack_bytes, 0);
            assert_eq!(stats.values, u64::from(Fixture::VALUES));
            assert_eq!(
                fixture.read_one().expect("relocated payload"),
                fixture.bytes
            );
        },
    );
}

/// Finish reclaiming unused pack space and shrink the storage index.
/// Starts with the same fragmentation as bounded cleanup, but completes all the work.
/// Preparing the fragmented store happens before timing.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn full_compaction(bencher: Bencher) {
    Fixture::bench(
        bencher,
        Fixture::fragmented,
        |fixture| fixture.database.compact(),
        |fixture, _| {
            let stats = fixture.database.statistics().expect("compacted statistics");
            assert_eq!(stats.dead_pack_bytes, 0);
            assert_eq!(stats.values, u64::from(Fixture::VALUES));
            assert_eq!(
                fixture.read_one().expect("compacted payload"),
                fixture.bytes
            );
        },
    );
}
