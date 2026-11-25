//! Legacy-log guard tests for the `gait workflow` CLI commands.
//!
//! Pins what the legacy-log guard (`LogError::Legacy { version }`) does to the
//! printed/returned values of list, log, abort and continue, and that the
//! on-disk bytes of a legacy log are never changed.

use gait_cli::cli::workflow::{
    cmd_workflow_abort, cmd_workflow_continue, workflow_list_json, workflow_log_lines,
};
use gait_core::proto::gait::workflow::v1::Workflow;
use gait_workflow::run_log::WorkflowRunLog;

/// The version-0 legacy fixture lines for `log.jsonl`. The last line is
/// intentionally torn (no trailing newline) — legacy logs may end mid-write.
fn legacy_fixture_lines() -> Vec<String> {
    vec![
        r#"{"version":0,"id":"a","seq":1,"workflowStart":{"runId":"legacy-run","workflow":"old","yamlPath":"old.yaml","def":{"name":"old"},"someUnknownKey":"x"}}"#.to_string(),
        r#"{"version":0,"id":"b","seq":2,"nodeStart":{"nodeId":"n1"}}"#.to_string(),
        r#"{"version":0,"id":"c","seq":3,"nodeCom"#.to_string(),
    ]
}

/// Write the legacy fixture into `<gait_dir>/workflow-runs/legacy-run/log.jsonl`.
fn write_legacy_fixture(gait_dir: &std::path::Path) {
    let run_dir = gait_dir.join("workflow-runs").join("legacy-run");
    std::fs::create_dir_all(&run_dir).unwrap();
    // Joining with "\n" leaves the torn last line with NO trailing newline.
    let content = legacy_fixture_lines().join("\n");
    std::fs::write(run_dir.join("log.jsonl"), content).unwrap();
}

/// Snapshot helper: read the raw bytes of the legacy log file.
fn legacy_log_bytes(gait_dir: &std::path::Path) -> Vec<u8> {
    std::fs::read(
        gait_dir
            .join("workflow-runs")
            .join("legacy-run")
            .join("log.jsonl"),
    )
    .unwrap()
}

#[test]
fn list_marks_a_legacy_log() {
    let td = tempfile::TempDir::new().unwrap();
    let gait_dir = td.path().to_path_buf();
    write_legacy_fixture(&gait_dir);

    // A second run with a normal, valid version-1 log must still list
    // normally, alongside the legacy one.
    let runs_dir = gait_dir.join("workflow-runs");
    let log = WorkflowRunLog::create(&runs_dir, "normal-run").unwrap();
    log.workflow_start("normal-run", "deploy", "wf.yaml", &[], &Workflow::default())
        .unwrap();

    let before = legacy_log_bytes(&gait_dir);
    let value = workflow_list_json(&gait_dir);
    assert_eq!(
        legacy_log_bytes(&gait_dir),
        before,
        "listing must not touch the legacy log's bytes"
    );

    let runs = value["runs"].as_array().expect("runs is a list");
    let legacy = runs
        .iter()
        .find(|r| r["run_id"] == "legacy-run")
        .expect("legacy run is listed");
    assert_eq!(
        legacy["status"], "legacy",
        "legacy run's status is 'legacy'"
    );
    assert_eq!(legacy["legacy"], true, "legacy run carries legacy: true");

    let normal = runs
        .iter()
        .find(|r| r["run_id"] == "normal-run")
        .expect("normal run is listed");
    assert_ne!(
        normal["status"], "legacy",
        "a valid version-1 log must not be marked legacy"
    );
}

#[test]
fn log_marks_a_legacy_log() {
    let td = tempfile::TempDir::new().unwrap();
    let gait_dir = td.path().to_path_buf();
    write_legacy_fixture(&gait_dir);

    let before = legacy_log_bytes(&gait_dir);
    let lines = workflow_log_lines(&gait_dir, "legacy-run");
    assert_eq!(
        legacy_log_bytes(&gait_dir),
        before,
        "logging must not touch the legacy log's bytes"
    );

    assert_eq!(
        lines.len(),
        1,
        "a legacy log prints exactly ONE marker line, got: {lines:?}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&lines[0]).expect("the marker line is valid JSON");
    assert_eq!(parsed["run_id"], "legacy-run");
    assert_eq!(parsed["legacy"], true);
    assert_eq!(parsed["version"], 0);
}

#[test]
fn abort_refuses_a_legacy_log() {
    let td = tempfile::TempDir::new().unwrap();
    let gait_dir = td.path().to_path_buf();
    write_legacy_fixture(&gait_dir);

    let before = legacy_log_bytes(&gait_dir);
    let err = cmd_workflow_abort(&["legacy-run".to_string()], gait_dir.clone(), false)
        .expect_err("abort must refuse a legacy run log");
    assert_eq!(
        legacy_log_bytes(&gait_dir),
        before,
        "a refused abort must not touch the legacy log's bytes"
    );
    let message = format!("{err:#}");
    let lowered = message.to_lowercase();
    assert!(
        lowered.contains("is a legacy log"),
        "abort error mentions legacy, got: {message}"
    );
    assert!(
        message.contains("legacy-run"),
        "abort error names the run id, got: {message}"
    );
}

#[tokio::test]
async fn continue_refuses_a_legacy_log() {
    let td = tempfile::TempDir::new().unwrap();
    let gait_dir = td.path().to_path_buf();
    write_legacy_fixture(&gait_dir);

    let before = legacy_log_bytes(&gait_dir);
    let err = cmd_workflow_continue(&["legacy-run".to_string()], gait_dir.clone())
        .await
        .expect_err("continue must refuse a legacy run log");
    assert_eq!(
        legacy_log_bytes(&gait_dir),
        before,
        "a refused continue must not touch the legacy log's bytes"
    );
    let message = format!("{err:#}");
    let lowered = message.to_lowercase();
    assert!(
        lowered.contains("is a legacy log"),
        "continue error mentions legacy, got: {message}"
    );
    assert!(
        message.contains("legacy-run"),
        "continue error names the run id, got: {message}"
    );
}

#[test]
fn read_errors_are_not_swallowed_by_abort() {
    let td = tempfile::TempDir::new().unwrap();
    let gait_dir = td.path().to_path_buf();
    let run_dir = gait_dir.join("workflow-runs").join("mid-corrupt-run");
    std::fs::create_dir_all(&run_dir).unwrap();
    // Line 1 is corrupt garbage; line 2 is a valid-looking event. Because the
    // corrupt line is NOT last, read_events must hard-error (not self-heal).
    std::fs::write(
        run_dir.join("log.jsonl"),
        "{\"version\":1,garbage\n{\"version\":1,\"id\":\"b\",\"seq\":2,\"kind\":null}\n",
    )
    .unwrap();

    let before = std::fs::read(run_dir.join("log.jsonl")).unwrap();
    let err = cmd_workflow_abort(&["mid-corrupt-run".to_string()], gait_dir.clone(), false)
        .expect_err("abort of a corrupt (non-tail) log must be an error, not Ok");
    assert_eq!(
        std::fs::read(run_dir.join("log.jsonl")).unwrap(),
        before,
        "a failed abort must leave the corrupt log byte-identical (no error event appended)"
    );
    let message = format!("{err:#}");
    assert!(
        message.contains("corrupt workflow run log"),
        "abort surfaces the read error instead of swallowing it, got: {message}"
    );
}
