use std::fmt;

use ajika_storage::{Compression, Database, Options};
use anyhow::Result;
use divan::{Bencher, black_box};

#[derive(Clone, Copy)]
pub(crate) enum Payload {
    Inline,
    Structured,
    Opaque,
    Large,
}

impl Payload {
    pub(crate) fn bytes(self) -> Vec<u8> {
        let length = match self {
            Self::Inline => 512,
            Self::Structured | Self::Opaque => 64 * 1024,
            Self::Large => 1024 * 1024,
        };
        if matches!(self, Self::Opaque) {
            // A fixed hash stream gives reproducible, poorly compressible bytes without a
            // random generator dependency or entropy acquisition in benchmark preparation.
            let mut bytes = vec![0; length];
            blake3::Hasher::new()
                .update(b"ajika benchmark payload")
                .finalize_xof()
                .fill(&mut bytes);
            bytes
        } else {
            // Names and repeated structure approximate serialized analysis data better than
            // an all-zero buffer, which would make compression unrealistically inexpensive.
            let block = (0..256)
                .map(|id| format!("symbol_{id:04}: module_{}\n", id % 16))
                .collect::<String>();
            block.bytes().cycle().take(length).collect()
        }
    }
}

impl fmt::Display for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Inline => "inline/512B",
            Self::Structured => "structured/64KiB",
            Self::Opaque => "opaque/64KiB",
            Self::Large => "structured/1MiB",
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Case {
    pub(crate) compression: Compression,
    pub(crate) payload: Payload,
}

impl Case {
    pub(crate) const INLINE: Self = Self {
        compression: Compression::None,
        payload: Payload::Inline,
    };
    pub(crate) const PACKED: Self = Self {
        compression: Compression::None,
        payload: Payload::Structured,
    };

    // Inline values bypass compression. Avoid three identically behaving inline cases.
    pub(crate) const ALL: [Self; 8] = [
        Self::INLINE,
        Self::PACKED,
        Self {
            compression: Compression::Lz4,
            payload: Payload::Structured,
        },
        Self {
            compression: Compression::Zstd,
            payload: Payload::Structured,
        },
        Self {
            compression: Compression::None,
            payload: Payload::Opaque,
        },
        Self {
            compression: Compression::Lz4,
            payload: Payload::Opaque,
        },
        Self {
            compression: Compression::Zstd,
            payload: Payload::Opaque,
        },
        Self {
            compression: Compression::Lz4,
            payload: Payload::Large,
        },
    ];
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}/{}", self.compression, self.payload)
    }
}

pub(crate) struct Fixture {
    pub(crate) database: Database,
    pub(crate) bytes: Vec<u8>,
    pub(crate) case: Case,
    _directory: tempfile::TempDir,
}

impl Fixture {
    pub(crate) const VALUES: u32 = 32;

    pub(crate) fn new(case: Case) -> Self {
        let directory = tempfile::tempdir().expect("storage benchmark directory");
        let database = Database::open(directory.path(), "bench-v1", Options::default())
            .expect("storage benchmark database");
        Self {
            database,
            bytes: case.payload.bytes(),
            case,
            _directory: directory,
        }
    }

    pub(crate) fn populated(case: Case) -> Self {
        let fixture = Self::new(case);
        fixture.write_batch().expect("populate storage");
        fixture
    }

    pub(crate) fn write_batch(&self) -> Result<()> {
        // Prepare a complete bounded batch before opening the sequential writer, as a host
        // would. This benchmark includes both preparation and publication, with one worker.
        let values = (0..Self::VALUES)
            .map(|_| self.case.compression.encode(self.bytes.clone()))
            .collect::<Result<Vec<_>>>()?;
        let mut write = self.database.begin_write()?;
        for (index, value) in values.into_iter().enumerate() {
            let id = Self::id(u32::try_from(index).expect("bounded batch index"));
            write.write_value(&id, value)?;
            write.set_metadata(&id, b"saved query record")?;
        }
        write.commit()
    }

    pub(crate) fn read_one(&self) -> Result<Vec<u8>> {
        self.database
            .snapshot()?
            .read_value(&Self::id(Self::VALUES / 2), self.bytes.len() as u64)
    }

    /// One pack holds the original batch. Replacing half of it leaves enough dead bytes for
    /// normal maintenance to select that pack, while its remaining neighbors must be copied.
    pub(crate) fn fragmented() -> Self {
        let fixture = Self::populated(Case::PACKED);
        {
            let mut write = fixture
                .database
                .begin_write()
                .expect("replacement checkpoint");
            for index in (0..Self::VALUES).step_by(2) {
                write
                    .write_value(
                        &Self::id(index),
                        Compression::None
                            .encode(fixture.bytes.clone())
                            .expect("raw payload"),
                    )
                    .expect("replace payload");
            }
            write.commit().expect("commit replacements");
        }
        assert!(
            fixture
                .database
                .statistics()
                .expect("fragmented statistics")
                .dead_pack_bytes
                >= 1024 * 1024
        );
        fixture
    }

    pub(crate) fn id(index: u32) -> [u8; 32] {
        let mut id = [0; 32];
        id[..4].copy_from_slice(&index.to_le_bytes());
        id
    }

    // As in the runtime suite, validate a separate rehearsal and exclude fixture destruction.
    pub(crate) fn bench<R>(
        bencher: Bencher,
        prepare: impl Fn() -> Self,
        work: impl Fn(&mut Self) -> Result<R>,
        check: impl Fn(&Self, &R),
    ) {
        {
            let mut fixture = prepare();
            let result = work(&mut fixture).expect("storage benchmark rehearsal");
            check(&fixture, &result);
        }
        bencher.with_inputs(prepare).bench_local_refs(|fixture| {
            black_box(work(fixture).expect("storage benchmark operation"))
        });
    }
}
