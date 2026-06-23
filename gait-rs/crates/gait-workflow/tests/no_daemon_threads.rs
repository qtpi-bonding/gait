#[test]
fn no_thread_spawn_in_workflow_runner() {
    let runner_src = include_str!("../src/runner.rs");
    let exec_prompt_src = include_str!("../src/exec_prompt.rs");
    let observer_src = include_str!("../src/observer.rs");
    let convert_src = include_str!("../src/convert.rs");

    let sources = [
        ("runner.rs", runner_src),
        ("exec_prompt.rs", exec_prompt_src),
        ("observer.rs", observer_src),
        ("convert.rs", convert_src),
    ];

    for (name, src) in &sources {
        assert!(
            !src.contains("thread::spawn"),
            "found thread::spawn in workflow source file: {}",
            name
        );
        assert!(
            !src.contains("Runtime::new"),
            "found Runtime::new in workflow source file: {}",
            name
        );
    }
}

#[test]
fn no_direct_embed_run_loop_in_repl() {
    // Check that gait-repl routes through orchestrator, not direct embed::run_loop
    // (static assertion: if this test runs and passes, the pattern is not in the source)
    let repl_src = include_str!("../../../crates/gait-repl/src/lib.rs");
    assert!(
        !repl_src.contains("embed::run_loop"),
        "found direct embed::run_loop call in gait-repl; should route through orchestrator"
    );
}
