use divan::Bencher;

use super::support::{Case, Fixture};

/// Encode an already serialized value for storage.
/// Measures compression, falling back to raw bytes when compression does not help,
/// and computing a checksum. Copying the input buffer happens before timing.
#[divan::bench(args = Case::ALL, sample_count = 10, sample_size = 1)]
fn encode(bencher: Bencher, case: Case) {
    let bytes = case.payload.bytes();
    bencher
        .with_inputs(|| bytes.clone())
        .bench_local_values(|bytes| {
            case.compression
                .encode(bytes)
                .expect("encode benchmark payload")
        });
}

/// Read one value from a populated database.
/// Includes finding its location, checking packed data, and decompressing when needed.
/// Populating the database happens before timing.
#[divan::bench(args = Case::ALL, sample_count = 10, sample_size = 1)]
fn read_value(bencher: Bencher, case: Case) {
    Fixture::bench(
        bencher,
        || Fixture::populated(case),
        |fixture| fixture.read_one(),
        |fixture, bytes| assert_eq!(bytes, &fixture.bytes),
    );
}
