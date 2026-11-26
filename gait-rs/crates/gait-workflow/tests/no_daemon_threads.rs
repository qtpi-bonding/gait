#[test]
fn no_thread_spawn_in_workflow_engine() {
    let script_src = include_str!("../src/script.rs");
    let host_src = include_str!("../src/host.rs");
    let script_turns_src = include_str!("../src/script_turns.rs");
    let turn_exec_src = include_str!("../src/turn_exec.rs");
    let capturing_sink_src = include_str!("../src/capturing_sink.rs");
    let launch_src = include_str!("../src/launch.rs");
    let observer_src = include_str!("../src/observer.rs");

    let sources = [
        ("script.rs", script_src),
        ("host.rs", host_src),
        ("script_turns.rs", script_turns_src),
        ("turn_exec.rs", turn_exec_src),
        ("capturing_sink.rs", capturing_sink_src),
        ("launch.rs", launch_src),
        ("observer.rs", observer_src),
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
