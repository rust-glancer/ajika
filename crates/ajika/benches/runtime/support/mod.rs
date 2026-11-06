//! Fixtures own the database and its temporary directory together. No production-only hooks
//! are needed: benchmarks use exactly the same public declarations and operations as a host.

pub(crate) mod workload;

use std::{fmt, sync::Arc, time::Duration};

use ajika::{Database, Retention};
use divan::{Bencher, black_box};

use self::workload::{Edit, Sources};

#[derive(Clone, Copy)]
pub(crate) enum Backend {
    Memory,
    Disk,
}

#[derive(Clone, Copy)]
pub(crate) struct Case {
    pub(crate) backend: Backend,
    pub(crate) files: u32,
}

impl Case {
    pub(crate) const ALL: [Self; 4] = [
        Self {
            backend: Backend::Memory,
            files: 32,
        },
        Self {
            backend: Backend::Memory,
            files: 512,
        },
        Self {
            backend: Backend::Disk,
            files: 32,
        },
        Self {
            backend: Backend::Disk,
            files: 512,
        },
    ];

    pub(crate) fn disk(files: u32) -> Self {
        Self {
            backend: Backend::Disk,
            files,
        }
    }

    pub(crate) fn query_count(self) -> u64 {
        // parse, analyze, and summary per file; one shared configuration and one index.
        u64::from(self.files) * 3 + 2
    }
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backend = match self.backend {
            Backend::Memory => "memory",
            Backend::Disk => "disk",
        };
        write!(f, "{backend}/{}", self.files)
    }
}

pub(crate) struct Fixture {
    // Declare the owner before the directory so the file lock is released before cleanup.
    pub(crate) database: Database,
    pub(crate) sources: Arc<Sources>,
    directory: Option<Arc<tempfile::TempDir>>,
}

impl Fixture {
    pub(crate) fn new(case: Case) -> Self {
        let sources = Arc::new(Sources::new(case.files));
        let directory = matches!(case.backend, Backend::Disk)
            .then(|| Arc::new(tempfile::tempdir().expect("benchmark directory")));
        let mut database = match &directory {
            Some(directory) => Database::open(
                directory.path(),
                "bench-v1",
                sources.saved.clone(),
                (),
                Self::retention(),
            )
            .expect("benchmark database"),
            None => Database::memory(sources.saved.clone(), (), Self::retention()),
        };
        // Query scheduling and checkpoint compression use the same worker limit. Fix both so
        // CPU simulation follows serial work rather than a scheduling-dependent thread trace.
        database.set_parallelism(1);
        Self {
            database,
            sources,
            directory,
        }
    }

    pub(crate) fn warm(case: Case) -> Self {
        let mut fixture = Self::new(case);
        let answer = fixture
            .database
            .run(workload::index)
            .expect("populate query graph");
        assert_eq!(*answer, fixture.sources.expected_index(Edit::Original));
        assert_eq!(fixture.database.statistics().executed, case.query_count());
        fixture
    }

    pub(crate) fn retention() -> Retention {
        Retention {
            max_entries: 100_000,
            max_bytes: 1024 * 1024 * 1024,
            max_idle: Duration::from_secs(24 * 60 * 60),
        }
    }

    /// Rehearse once to verify the scenario, then give every measured iteration a fresh fixture.
    /// Borrowing inputs keeps database shutdown and directory cleanup outside measurement too.
    /// The closures at each call site show preparation, measured work, and expected behavior.
    pub(crate) fn bench<R>(
        bencher: Bencher,
        prepare: impl Fn() -> Self,
        work: impl Fn(&mut Self) -> ajika::Result<R>,
        check: impl Fn(&Self, &R),
    ) {
        {
            let mut fixture = prepare();
            let result = work(&mut fixture).expect("benchmark rehearsal");
            check(&fixture, &result);
        }
        bencher
            .with_inputs(prepare)
            .bench_local_refs(|fixture| black_box(work(fixture).expect("benchmark operation")));
    }
}

/// A closed, populated cache. Opening it creates fresh process-local validity state while
/// keeping fixture generation and the original checkpoint outside the measured request.
pub(crate) struct SavedCache {
    sources: Arc<Sources>,
    directory: Arc<tempfile::TempDir>,
}

impl SavedCache {
    pub(crate) fn new(files: u32) -> Self {
        let Fixture {
            database,
            sources,
            directory,
        } = Fixture::warm(Case::disk(files));
        drop(database);
        Self {
            sources,
            directory: directory.expect("disk fixture"),
        }
    }

    pub(crate) fn open(&self) -> Fixture {
        let mut database = Database::open(
            self.directory.path(),
            "bench-v1",
            self.sources.saved.clone(),
            (),
            Fixture::retention(),
        )
        .expect("reopen benchmark cache");
        database.set_parallelism(1);
        Fixture {
            database,
            sources: self.sources.clone(),
            directory: Some(self.directory.clone()),
        }
    }
}
