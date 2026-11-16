#![cfg(feature = "telemetry")]

#[test]
fn report_emits_one_coded_error_record_with_the_text_under_the_default_mode() {
    let cap = concord_telemetry::testing::capture_exclusive();
    let error = gait_cli::cli::error::CliError::not_found("secret-token-xyz");
    gait_bin::report(&error);
    let records = cap.errors_where(|record| record.code == "gait.cli.not_found");
    assert_eq!(records.len(), 1);
    // CONCORD_ERROR_TEXT defaults to full: the message text is recorded.
    assert!(records[0]
        .fields
        .values()
        .any(|value| format!("{value:?}").contains("secret-token-xyz")));
}

use std::process::ExitCode;

type Capture = concord_telemetry::testing::ExclusiveCapture;

fn all_errors(cap: &Capture) -> Vec<concord_telemetry::testing::ErrorRecord> {
    cap.errors_where(|_| true)
}

fn git_init(dir: &std::path::Path) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "-q"])
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn a_marked_error_is_printed_and_coded_the_same_but_not_recorded_again() {
    let cap = concord_telemetry::testing::capture_exclusive();
    let plain = gait_cli::cli::error::CliError::not_found("nothing here");
    let plain_text = gait_cli::cli::error::text(&plain);
    let plain_exit = gait_bin::report(&plain);
    assert_eq!(all_errors(&cap).len(), 1);
    drop(cap);

    let cap = concord_telemetry::testing::capture_exclusive();
    let marked = gait_cli::cli::error::mark_recorded(gait_cli::cli::error::CliError::not_found(
        "nothing here",
    ));
    assert!(gait_cli::cli::error::is_recorded(&marked));
    assert_eq!(gait_cli::cli::error::text(&marked), plain_text);
    assert_eq!(gait_bin::report(&marked), plain_exit);
    assert!(all_errors(&cap).is_empty(), "{:?}", all_errors(&cap));
}

#[test]
fn a_cli_level_error_still_gets_its_one_gait_cli_record() {
    let cap = concord_telemetry::testing::capture_exclusive();
    let error = gait_cli::cli::error::CliError::usage("missing --thing");
    assert_eq!(gait_bin::report(&error), ExitCode::from(65));
    let records = all_errors(&cap);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].code, "gait.cli.usage");
    assert_eq!(records[0].source, "gait.cli");
}

/// A failed turn driven end to end (a real brain process whose provider is
/// unreachable) is recorded once, by the turn, not again by the CLI.
#[tokio::test]
async fn a_failed_turn_run_yields_one_error_record_the_turns() {
    let cap = concord_telemetry::testing::capture_exclusive();
    let root = tempfile::tempdir().unwrap();
    let gait_dir = root.path().join("home");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(gait_dir.join("inferences/m")).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    git_init(&gait_dir);
    git_init(&repo);
    std::fs::write(
        gait_dir.join("inferences/m/inference.textproto"),
        "model: \"lmstudio/x\"\ncompaction_threshold_tokens: 128000\n",
    )
    .unwrap();
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "m" }"#).unwrap();
    // SAFETY: only this test in this binary reads these, before it spawns anything.
    unsafe {
        std::env::set_var("GAIT_BRAIN_EXE", env!("CARGO_BIN_EXE_gait"));
        std::env::set_var("GAIT_HOME", &gait_dir);
        std::env::set_var("LMSTUDIO_BASE_URL", "http://127.0.0.1:1/v1");
    }
    let argv: Vec<String> = ["--task", "hi", "--inference", "m"].map(String::from).to_vec();
    let error = gait_cli::cli::run::cmd_run(&argv, gait_dir.clone(), repo.clone())
        .await
        .expect_err("the turn fails");
    assert_eq!(
        gait_cli::cli::error::text(&error),
        "[gait] run failed; see the diagnostic above"
    );
    assert_eq!(gait_bin::report(&error), ExitCode::from(69));
    let records = all_errors(&cap);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].source, "gait.turn");
    assert_eq!(records[0].code, "gait.provider.connect");
    assert_eq!(records[0].class, "transient");
}
