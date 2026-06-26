// crates/gait-cli/tests/exec_no_gait_dir.rs
#[tokio::test]
async fn run_one_off_resolves_without_gait_dir() {
    // The stub brain is selected by the same env hook the spine tests use.
    std::env::set_var("ANTHROPIC_API_KEY", "test");
    let cwd = std::env::current_dir().unwrap();
    let r = gait_cli::cli::exec::run_one_off(
        "untar a.tar",
        Some("anthropic/claude-haiku-4-5-20251001".into()),
        &cwd,
    )
    .await;
    assert!(matches!(
        r.unwrap(),
        gait_cli::cli::exec::OneOff::Cmd(_) | gait_cli::cli::exec::OneOff::Output(_)
    ));
}
