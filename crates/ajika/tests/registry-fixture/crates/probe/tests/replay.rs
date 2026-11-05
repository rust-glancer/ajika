use std::process::Command;

#[test]
fn linked_queries_replay_after_restart() {
    let directory = tempfile::tempdir().expect("database directory");
    for step in 0..3 {
        let output = Command::new(env!("CARGO_BIN_EXE_registry-probe"))
            .arg(step.to_string())
            .arg(directory.path().join("queries"))
            .output()
            .expect("probe starts");
        assert!(
            output.status.success(),
            "step {step}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn no_linked_providers_means_no_registrations() {
    let output = Command::new(env!("CARGO_BIN_EXE_empty-registry"))
        .output()
        .expect("probe starts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
