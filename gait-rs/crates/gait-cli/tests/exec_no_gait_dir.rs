// crates/gait-cli/tests/exec_no_gait_dir.rs
// NOTE: this test requires a mock brain (via CARGO_BIN_EXE_mock_brain env hook) and
// a real gait binary as current_exe(). It is ignored until that infrastructure is wired
// up (see spine_ephemeral.rs for the correct integration test path).
#[tokio::test]
#[ignore]
async fn run_one_off_resolves_named_inference_from_gait_dir() {
    // The stub brain is selected by the same env hook the spine tests use.
    std::env::set_var("ANTHROPIC_API_KEY", "test");
    let cwd = std::env::current_dir().unwrap();
    let gait_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(gait_dir.path().join("inferences/test")).unwrap();
    std::fs::write(
        gait_dir.path().join("inferences/test/INFERENCE.yaml"),
        "model: anthropic/claude-haiku-4-5-20251001\ncontext_window_tokens: 128000\n",
    )
    .unwrap();
    let r =
        gait_cli::cli::exec::run_one_off("untar a.tar", Some("test".into()), gait_dir.path(), &cwd)
            .await;
    assert!(matches!(
        r.unwrap(),
        gait_cli::cli::exec::OneOff::Cmd(_) | gait_cli::cli::exec::OneOff::Output(_)
    ));
}
