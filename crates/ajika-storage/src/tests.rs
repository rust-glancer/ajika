use anyhow::Result;

use crate::{Compression, Database, Options};

#[test]
fn published_values_round_trip_in_each_encoding() -> Result<()> {
    let values = [
        Vec::new(),
        b"small inline value".to_vec(),
        "fn repeated_name() {}\n".repeat(10_000).into_bytes(),
    ];
    for compression in [Compression::None, Compression::Lz4, Compression::Zstd] {
        let root = tempfile::tempdir()?;
        let options = Options {
            compression,
            ..Options::default()
        };
        let database = Database::open(root.path(), "fixture", options)?;
        let mut write = database.begin_write()?;
        write.set_identity("files", 0, b"src/lib.rs")?;
        for (id, value) in values.iter().enumerate() {
            write.write_value(&[id as u8; 32], compression.encode(value.clone())?)?;
        }
        write.set_metadata(&[1; 32], b"complete output list")?;
        write.commit()?;
        let stats = database.statistics()?;
        assert_eq!(stats.values, 3);
        assert_eq!(stats.pack_files, 1);
        if compression != Compression::None {
            assert!(stats.packed_value_bytes < values[2].len() as u64 / 2);
        }
        drop(database);

        let database = Database::open(root.path(), "fixture", options)?;
        let read = database.snapshot()?;
        assert_eq!(
            read.identities()?,
            vec![("files".to_owned(), 0, b"src/lib.rs".to_vec())]
        );
        assert_eq!(
            read.metadata(&[1; 32])?.as_deref(),
            Some(b"complete output list".as_slice())
        );
        for (id, value) in values.iter().enumerate() {
            assert_eq!(
                read.read_value(&[id as u8; 32], value.len() as u64)?,
                *value
            );
        }
    }
    Ok(())
}

#[test]
fn only_committed_replacements_become_visible() -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = Database::open(root.path(), "fixture", Options::default())?;
    let old = vec![1; 32_000];
    let new = vec![2; 64_000];
    let mut write = database.begin_write()?;
    write.write_value(&[1; 32], Compression::None.encode(old.clone())?)?;
    write.commit()?;

    let mut pending = database.begin_write()?;
    pending.write_value(&[1; 32], Compression::Lz4.encode(new.clone())?)?;
    drop(pending);
    assert_eq!(
        database
            .snapshot()?
            .read_value(&[1; 32], old.len() as u64)?,
        old
    );
    assert_eq!(
        database.statistics()?.pack_files,
        1,
        "abandoned file was removed"
    );

    let mut write = database.begin_write()?;
    write.write_value(&[1; 32], Compression::Lz4.encode(new.clone())?)?;
    write.commit()?;
    assert_eq!(
        database
            .snapshot()?
            .read_value(&[1; 32], new.len() as u64)?,
        new
    );
    assert_eq!(
        database.statistics()?.pack_files,
        1,
        "fully replaced file was removed"
    );
    Ok(())
}

#[test]
fn removing_neighbors_keeps_live_frames_until_the_pack_is_empty() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut database = Database::open(root.path(), "fixture", Options::default())?;
    let value = vec![42; 10_000];
    let mut write = database.begin_write()?;
    write.set_identity("files", 0, b"src/lib.rs")?;
    for id in [1, 2] {
        write.write_value(&[id; 32], Compression::None.encode(value.clone())?)?;
    }
    write.commit()?;
    let mut write = database.begin_write()?;
    write.remove_value(&[1; 32])?;
    write.commit()?;
    assert_eq!(database.snapshot()?.read_value(&[2; 32], 10_000)?, value);
    assert!(database.statistics()?.dead_pack_bytes >= 10_000);

    database.invalidate()?;
    let stats = database.statistics()?;
    assert_eq!((stats.values, stats.pack_files), (0, 0));
    assert_eq!(
        database.snapshot()?.identities()?.len(),
        1,
        "query invalidation preserves identities"
    );
    Ok(())
}

#[test]
fn compaction_resumes_after_restart_without_changing_values() -> Result<()> {
    let root = tempfile::tempdir()?;
    let options = Options {
        compression: Compression::None,
        ..Options::default()
    };
    let database = Database::open(root.path(), "fixture", options)?;
    let value = vec![42; 2 * 1024 * 1024];
    let mut write = database.begin_write()?;
    for id in 0..12 {
        write.write_value(&[id; 32], Compression::None.encode(value.clone())?)?;
    }
    write.commit()?;
    let mut write = database.begin_write()?;
    for id in 0..4 {
        write.remove_value(&[id; 32])?;
    }
    write.commit()?;

    database.maintain()?;
    assert!(
        database.statistics()?.dead_pack_bytes > 0,
        "one pass does not scan the whole pack"
    );
    drop(database);
    let database = Database::open(root.path(), "fixture", options)?;
    database.maintain()?;
    assert_eq!(
        database.statistics()?.dead_pack_bytes,
        0,
        "the resumed pass retires the source pack"
    );
    for id in 4..12 {
        assert_eq!(
            database
                .snapshot()?
                .read_value(&[id; 32], value.len() as u64)?,
            value
        );
    }
    Ok(())
}

#[test]
fn relocation_preserves_compression_when_the_write_policy_changes() -> Result<()> {
    for compression in [Compression::Lz4, Compression::Zstd] {
        let root = tempfile::tempdir()?;
        let mut database = Database::open(root.path(), "fixture", Options::default())?;
        let value = b"some repeated query output\n".repeat(10_000);
        let mut write = database.begin_write()?;
        for id in [1, 2] {
            write.write_value(&[id; 32], compression.encode(value.clone())?)?;
        }
        write.commit()?;
        let mut write = database.begin_write()?;
        write.remove_value(&[1; 32])?;
        write.commit()?;
        let live_bytes = database.statistics()?.packed_value_bytes;
        database.set_compression(Compression::None);
        database.compact()?;
        let after = database.statistics()?;
        assert_eq!(
            after.packed_value_bytes, live_bytes,
            "copy the compressed bytes unchanged"
        );
        assert_eq!(after.dead_pack_bytes, 0);
        assert_eq!(
            database
                .snapshot()?
                .read_value(&[2; 32], value.len() as u64)?,
            value
        );
    }
    Ok(())
}

#[test]
fn small_writes_consolidate_and_repeated_replacements_settle() -> Result<()> {
    const KEYS: u8 = 32;
    let root = tempfile::tempdir()?;
    let mut database = Database::open(root.path(), "fixture", Options::default())?;
    let mut peak_files = 0;
    for turn in 0..3 * KEYS {
        let id = turn % KEYS;
        let mut write = database.begin_write()?;
        write.write_value(&[id; 32], Compression::None.encode(vec![turn; 16 * 1024])?)?;
        write.commit()?;
        database.maintain()?;
        peak_files = peak_files.max(database.statistics()?.pack_files);
        if turn + 1 == 2 * KEYS {
            drop(database);
            database = Database::open(root.path(), "fixture", Options::default())?;
        }
    }
    assert!(
        peak_files < u64::from(KEYS),
        "consolidation bounds files during repeated writes: {peak_files}"
    );
    database.compact()?;
    let stats = database.statistics()?;
    assert_eq!((stats.values, stats.dead_pack_bytes), (u64::from(KEYS), 0));
    assert_eq!(stats.pack_files, 1);
    let read = database.snapshot()?;
    for id in 0..KEYS {
        assert_eq!(
            read.read_value(&[id; 32], 16 * 1024)?,
            vec![2 * KEYS + id; 16 * 1024]
        );
    }
    Ok(())
}

#[test]
fn an_exhausted_budget_still_allows_reclamation_and_reads() -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = Database::open(root.path(), "fixture", Options::default())?;
    let mut write = database.begin_write()?;
    for id in [1, 2] {
        write.write_value(&[id; 32], Compression::None.encode(vec![id; 32_000])?)?;
    }
    write.commit()?;
    drop(database);

    let mut database = Database::open(
        root.path(),
        "fixture",
        Options {
            max_physical_bytes: 1,
            ..Options::default()
        },
    )?;
    let mut pending = database.begin_write()?;
    assert_eq!(
        pending
            .write_value(&[3; 32], Compression::None.encode(vec![3; 32_000])?)
            .expect_err("new values exceed the budget")
            .kind(),
        crate::ErrorKind::Capacity
    );
    drop(pending);
    let mut write = database.begin_write()?;
    write.remove_value(&[1; 32])?;
    write.commit()?;
    database.compact()?;
    assert_eq!(database.statistics()?.dead_pack_bytes, 0);
    assert_eq!(
        database.snapshot()?.read_value(&[2; 32], 32_000)?,
        vec![2; 32_000]
    );
    Ok(())
}

#[test]
fn abrupt_exit_keeps_a_complete_published_checkpoint() -> Result<()> {
    for published in [false, true] {
        let root = tempfile::tempdir()?;
        let database = Database::open(root.path(), "fixture", Options::default())?;
        let mut write = database.begin_write()?;
        write.write_value(&[1; 32], Compression::Lz4.encode(vec![1; 32_000])?)?;
        write.set_metadata(&[1; 32], b"old")?;
        write.commit()?;
        drop(database);

        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--ignored", "--exact", "tests::interrupted_storage_worker"])
            .env("AJIKA_TEST_STORAGE", root.path())
            .env("AJIKA_TEST_PUBLISHED", if published { "yes" } else { "no" })
            .status()?;
        assert_eq!(
            status.code(),
            Some(73),
            "worker exits without running destructors"
        );
        let database = Database::open(root.path(), "fixture", Options::default())?;
        let read = database.snapshot()?;
        assert_eq!(
            read.read_value(&[1; 32], 32_000)?,
            vec![if published { 2 } else { 1 }; 32_000]
        );
        assert_eq!(
            read.metadata(&[1; 32])?.as_deref(),
            Some(if published { b"new" } else { b"old" }.as_slice())
        );
        assert_eq!(
            database.statistics()?.pack_files,
            1,
            "reopen discards abandoned packs"
        );
    }
    Ok(())
}

#[test]
#[ignore = "child process for abrupt_exit_keeps_a_complete_published_checkpoint"]
fn interrupted_storage_worker() -> Result<()> {
    let root = std::env::var_os("AJIKA_TEST_STORAGE").expect("parent supplies storage path");
    let database = Database::open(std::path::Path::new(&root), "fixture", Options::default())?;
    let mut write = database.begin_write()?;
    write.write_value(&[1; 32], Compression::Zstd.encode(vec![2; 32_000])?)?;
    write.set_metadata(&[1; 32], b"new")?;
    if std::env::var("AJIKA_TEST_PUBLISHED")?.as_str() == "yes" {
        write.commit()?;
    } else {
        // The new payload is complete on disk, but its index transaction is still private.
        write.packs.finish()?;
    }
    std::process::exit(73);
}

#[test]
fn reloading_a_stopped_owner_keeps_its_checkpoint_and_lock() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut database = Database::open(root.path(), "fixture", Options::default())?;
    let write = database.begin_write()?;
    write.set_metadata(&[1; 32], b"complete checkpoint")?;
    write.commit()?;

    // Model the state after an uncertain commit. The published catalog remains on disk.
    database
        .healthy
        .store(false, std::sync::atomic::Ordering::Release);
    database.reopen()?;
    assert_eq!(
        database.snapshot()?.metadata(&[1; 32])?.as_deref(),
        Some(b"complete checkpoint".as_slice())
    );
    assert!(matches!(
        Database::open(root.path(), "fixture", Options::default()),
        Err(error) if error.kind() == crate::ErrorKind::InUse
    ));
    Ok(())
}

#[test]
fn opening_rebuilds_a_damaged_index() -> Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("index.redb"), b"damaged header")?;

    let database = Database::open(root.path(), "fixture", Options::default())?;
    let write = database.begin_write()?;
    write.set_metadata(&[1; 32], b"rebuilt checkpoint")?;
    write.commit()?;
    assert_eq!(
        database.snapshot()?.metadata(&[1; 32])?.as_deref(),
        Some(b"rebuilt checkpoint".as_slice())
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn rebuilding_an_empty_index_restores_supplied_identities() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut database = Database::open(root.path(), "fixture", Options::default())?;
    let index = root.path().join("index.redb");
    // Move the old inode aside so closing its redb handle cannot change the empty file.
    std::fs::rename(&index, root.path().join("previous.redb"))?;
    std::fs::File::create(index)?;

    database.rebuild(&[crate::Identity {
        namespace: "files",
        id: 0,
        key: b"src/lib.rs",
    }])?;
    drop(database);
    let database = Database::open(root.path(), "fixture", Options::default())?;
    assert_eq!(
        database.snapshot()?.identities()?,
        vec![("files".into(), 0, b"src/lib.rs".to_vec())]
    );
    Ok(())
}
