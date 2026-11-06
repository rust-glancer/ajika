use divan::Bencher;

use super::support::{Case, Fixture};

/// Encode and store 32 values and their records in one checkpoint.
/// Includes preparing the value buffers and committing their new locations.
#[divan::bench(args = Case::ALL, sample_count = 10, sample_size = 1)]
fn write_batch(bencher: Bencher, case: Case) {
    Fixture::bench(
        bencher,
        || Fixture::new(case),
        |fixture| fixture.write_batch(),
        |fixture, _| {
            let stats = fixture
                .database
                .statistics()
                .expect("checkpoint statistics");
            assert_eq!(stats.values, u64::from(Fixture::VALUES));
            assert_eq!(stats.records, u64::from(Fixture::VALUES));
            assert_eq!(
                stats.raw_value_bytes,
                u64::from(Fixture::VALUES) * fixture.bytes.len() as u64
            );
            assert_eq!(fixture.read_one().expect("read checkpoint"), fixture.bytes);
        },
    );
}

/// Update saved query records while leaving their value bytes in place.
/// Represents recomputation where answers stay equal but dependencies change.
#[divan::bench(args = [Case::INLINE, Case::PACKED], sample_count = 10, sample_size = 1)]
fn metadata_only(bencher: Bencher, case: Case) {
    Fixture::bench(
        bencher,
        || Fixture::populated(case),
        |fixture| {
            let write = fixture.database.begin_write()?;
            for index in 0..Fixture::VALUES {
                write.set_metadata(&Fixture::id(index), b"updated query record")?;
            }
            write.commit()
        },
        |fixture, _| {
            let read = fixture.database.snapshot().expect("checkpoint snapshot");
            assert_eq!(
                read.metadata(&Fixture::id(0))
                    .expect("updated record")
                    .as_deref(),
                Some(b"updated query record".as_slice())
            );
            assert_eq!(
                fixture.read_one().expect("unchanged payload"),
                fixture.bytes
            );
            assert_eq!(
                fixture
                    .database
                    .statistics()
                    .expect("checkpoint statistics")
                    .dead_pack_bytes,
                0
            );
        },
    );
}
