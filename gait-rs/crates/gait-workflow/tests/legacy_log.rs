//! Tests for the "legacy workflow log" refusal: logs written before the
//! format gained a `version` field (version 0) must be refused with a
//! typed `LogError::Legacy { version }` rather than being parsed, self-healed,
//! or rewritten.
//!
//! Every path (open, read_events, emit, status) must leave the file
//! byte-identical.

use std::path::Path;

use gait_workflow::run_log::{LogError, WorkflowRunLog};
use gait_workflow::status::workflow_status_block;

/// The exact bytes of the legacy fixture log. Two version-0 events (with an
/// old envelope shape: `def`/`yamlPath`, plus one unknown key) and a torn
/// final line with no trailing newline. Tests assert the on-disk file stays
/// byte-identical to this — proving no rewrite or self-heal ran.
fn legacy_log_bytes() -> Vec<u8> {
    let line1 = r#"{"version":0,"id":"a","seq":1,"workflowStart":{"runId":"legacy-run","workflow":"old","yamlPath":"old.yaml","def":{"name":"old"},"someUnknownKey":"x"}}"#;
    let line2 = r#"{"version":0,"id":"b","seq":2,"nodeStart":{"nodeId":"n1"}}"#;
    let torn = r#"{"version":0,"id":"c","seq":3,"nodeCom"#;
    format!("{line1}\n{line2}\n{torn}").into_bytes()
}

/// Write `bytes` as the log.jsonl of `run_id` under `runs_dir`.
fn write_raw_log(runs_dir: &Path, run_id: &str, bytes: &[u8]) {
    let run_dir = runs_dir.join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("log.jsonl"), bytes).unwrap();
}

/// Set up test 2/3/6 style fixtures: a `runs` dir where a handle was created
/// for a sacrificial run id, then the legacy bytes were written over its
/// log.jsonl. Returns (tempdir, log handle, legacy bytes).
fn legacy_handle_setup(run_id: &str) -> (tempfile::TempDir, WorkflowRunLog, Vec<u8>) {
    let dir = tempfile::TempDir::new().unwrap();
    let log = WorkflowRunLog::create(dir.path(), run_id).unwrap();
    let bytes = legacy_log_bytes();
    // Overwrite the freshly created (empty) log with the legacy fixture.
    std::fs::write(log.path(), &bytes).unwrap();
    (dir, log, bytes)
}

#[test]
fn open_a_legacy_log_is_legacy() {
    let dir = tempfile::TempDir::new().unwrap();
    let bytes = legacy_log_bytes();
    write_raw_log(dir.path(), "legacy-run", &bytes);

    let err = WorkflowRunLog::open(dir.path(), "legacy-run")
        .expect_err("opening a legacy (version 0) log must be refused");
    let legacy = err
        .downcast_ref::<LogError>()
        .expect("error must downcast to LogError");
    assert!(
        matches!(legacy, LogError::Legacy { version: 0 }),
        "expected LogError::Legacy {{ version: 0 }}, got: {legacy:?}"
    );

    let on_disk = std::fs::read(dir.path().join("legacy-run/log.jsonl")).unwrap();
    assert_eq!(on_disk, bytes, "refusing open must not touch the file");
}

#[test]
fn read_events_on_a_legacy_log_is_legacy_and_does_not_heal_the_torn_tail() {
    let (_dir, log, bytes) = legacy_handle_setup("holder");

    let err = log
        .read_events()
        .expect_err("reading a legacy log must be refused");
    let legacy = err
        .downcast_ref::<LogError>()
        .expect("error must downcast to LogError");
    assert!(
        matches!(legacy, LogError::Legacy { version: 0 }),
        "expected LogError::Legacy {{ version: 0 }}, got: {legacy:?}"
    );

    let on_disk = std::fs::read(log.path()).unwrap();
    assert_eq!(
        on_disk, bytes,
        "read_events must not self-heal or rewrite a legacy log"
    );
}

#[test]
fn emit_on_a_legacy_log_is_legacy() {
    let (_dir, log, bytes) = legacy_handle_setup("holder");

    let err = log
        .node_start("n1", "shell", 1, "session-1")
        .expect_err("emitting to a legacy log must be refused");
    let legacy = err
        .downcast_ref::<LogError>()
        .expect("error must downcast to LogError");
    assert!(
        matches!(legacy, LogError::Legacy { version: 0 }),
        "expected LogError::Legacy {{ version: 0 }}, got: {legacy:?}"
    );

    let on_disk = std::fs::read(log.path()).unwrap();
    assert_eq!(on_disk, bytes, "a refused emit must not touch the file");
}

#[test]
fn status_block_skips_a_legacy_log() {
    let dir = tempfile::TempDir::new().unwrap();
    let bytes = legacy_log_bytes();
    write_raw_log(dir.path(), "legacy-run", &bytes);

    let block = workflow_status_block(dir.path());
    assert!(
        !block.contains("legacy-run"),
        "status block must not mention a legacy run, got: {block}"
    );

    let on_disk = std::fs::read(dir.path().join("legacy-run/log.jsonl")).unwrap();
    assert_eq!(on_disk, bytes, "status scan must not touch the file");
}

#[test]
fn a_version_one_log_is_not_legacy() {
    let dir = tempfile::TempDir::new().unwrap();

    // Current-format log: create + writer round-trips fine.
    let log = WorkflowRunLog::create(dir.path(), "modern-run").unwrap();
    let def = gait_core::proto::gait::workflow::v1::Workflow::default();
    log.workflow_start("modern-run", "wf", "wf.yaml", &[], &def)
        .unwrap();
    drop(log);

    let reopened = WorkflowRunLog::open(dir.path(), "modern-run").unwrap();
    let events = reopened.read_events().unwrap();
    assert_eq!(events.len(), 1, "current logs must still read back");

    // A hand-written first line with "version":1 is accepted too.
    let hand_dir = tempfile::TempDir::new().unwrap();
    let hand_bytes = concat!(
        r#"{"version":1,"id":"h","seq":1,"workflowStart":{"runId":"hand-run","workflow":"wf","yamlPath":"wf.yaml","def":{"name":"wf"}}}"#,
        "\n",
    );
    write_raw_log(hand_dir.path(), "hand-run", hand_bytes.as_bytes());
    let hand_log = WorkflowRunLog::open(hand_dir.path(), "hand-run").unwrap();
    let hand_events = hand_log.read_events().unwrap();
    assert_eq!(hand_events.len(), 1, "a version:1 line must be accepted");
}

#[test]
fn a_log_with_no_version_key_at_all_is_version_zero() {
    let dir = tempfile::TempDir::new().unwrap();
    let log = WorkflowRunLog::create(dir.path(), "holder").unwrap();
    let bytes: &[u8] = b"{\"id\":\"a\",\"seq\":1}\n";
    std::fs::write(log.path(), bytes).unwrap();

    let err = log
        .read_events()
        .expect_err("a log with no version key is version 0, hence legacy");
    let legacy = err
        .downcast_ref::<LogError>()
        .expect("error must downcast to LogError");
    assert!(
        matches!(legacy, LogError::Legacy { version: 0 }),
        "expected LogError::Legacy {{ version: 0 }}, got: {legacy:?}"
    );

    let on_disk = std::fs::read(log.path()).unwrap();
    assert_eq!(on_disk, bytes, "refusing must not touch the file");
}
