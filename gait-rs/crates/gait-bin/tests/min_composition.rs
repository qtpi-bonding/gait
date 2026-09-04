//! Proves `gait-min`'s command surface: full-only `workflow`/`local` are
//! genuinely absent, while the commands `gait-manual/manual.yaml`'s ungated
//! guides need (`init`, `sessions`, `config`, `auth`, `inference`, `models`,
//! `-i`, `-e`, a bare positional task) — and the skill/plugin admin commands
//! that stay unconditional in `gait-cli` (`register`/`unregister`/`load`/
//! `unload`/`enable`/`disable`) — are present.
//!
//! Only runs when built with `--no-default-features --features min`, since
//! `CARGO_BIN_EXE_gait-min` only exists for that composition (the `gait-min`
//! `[[bin]]` has `required-features = ["min"]`).
#![cfg(feature = "min")]

use clap::CommandFactory;
use gait_bin::manual::{render_manual, ManualFlavor, ManualInput};
use gait_bin::min_cli::MinCli;
use gait_repl::plain::MIN_REPL_COMMANDS;

fn run_gait_min(arg: &str) -> String {
    let exe = env!("CARGO_BIN_EXE_gait-min");
    let output = std::process::Command::new(exe)
        .arg(arg)
        .output()
        .expect("gait-min should run");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn min_help_excludes_workflow_and_local() {
    let help = run_gait_min("--help");

    // Positive: the Min command surface manual.yaml and Task 4's
    // MIN_REPL_COMMANDS both depend on.
    assert!(help.contains("Interactive REPL"), "missing -i: {help}");
    assert!(help.contains("-e, --exec"), "missing -e: {help}");
    assert!(help.contains("[TASK]"), "missing positional task: {help}");
    assert!(help.contains("init"), "missing init: {help}");
    assert!(help.contains("sessions"), "missing sessions: {help}");
    assert!(help.contains("config"), "missing config: {help}");
    assert!(help.contains("auth"), "missing auth: {help}");
    assert!(help.contains("inference"), "missing inference: {help}");
    assert!(help.contains("models"), "missing models: {help}");

    // register/enable/disable/load/unload are intentionally still present —
    // they stay unconditional in gait-cli (skill/plugin admin, not
    // workflow- or local-specific).
    for still_present in ["register", "unregister", "enable", "disable", "load", "unload"] {
        assert!(
            help.contains(still_present),
            "expected {still_present} to remain present in gait-min: {help}"
        );
    }

    // Negative: genuinely full-only. A bare `contains("workflow")` would
    // false-positive on `config`'s about-text, which mentions
    // `cmd_workflow` incidentally — match the actual command-table row
    // shape (`\n  workflow    ...`) clap renders for a real `workflow`
    // subcommand instead.
    assert!(
        !help.contains("\n  workflow "),
        "gait-min must not expose the workflow subcommand: {help}"
    );
    // A generic `contains("local")` would false-positive on incidental text
    // (e.g. "local session"); match the real clap about-text the full
    // binary's `Local` subcommand renders instead
    // (`gait local setup --model-path /path/to/model.gguf`,
    // crates/gait-cli/src/cli/admin.rs's `LocalArgs` doc comment).
    assert!(
        !help.contains("gait local setup"),
        "gait-min must not expose the local subcommand: {help}"
    );
}

#[test]
fn min_manual_help_excludes_install() {
    let exe = env!("CARGO_BIN_EXE_gait-min");
    let output = std::process::Command::new(exe)
        .args(["manual", "--help"])
        .output()
        .expect("gait-min manual --help should run");
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("print"), "expected `print`: {help}");
    assert!(
        !help.contains("install"),
        "gait-min manual --help must not expose `install`: {help}"
    );
}

/// Task 8, Step 1: renders the Min manual using the Min command and REPL
/// registries. Confirms `init`, `/new`, and sandbox docs are present, and
/// that the full-only "Workflows and extensions" guide (and anything it
/// alone would pull in) is absent.
#[test]
fn min_manual_omits_workflows_and_extensions() {
    let rendered = render_manual(ManualInput {
        program_name: "gait-min",
        cli: MinCli::command(),
        repl_commands: MIN_REPL_COMMANDS,
        flavor: ManualFlavor::Min,
    })
    .unwrap();
    assert!(rendered.contains("#### `gait-min init`"));
    assert!(rendered.contains("#### `/new`"));
    assert!(rendered.contains("`sandbox`"));
    assert!(!rendered.contains("Workflows and extensions"));
    assert!(!rendered.contains("gait-min workflow"));
    assert!(!rendered.contains("/workflow"));

    // Skills are available in both builds: the ungated Skills guide must
    // still render in the Min manual.
    assert!(rendered.contains("## Skills"));
    assert!(rendered.contains("#### `/skill"));
}

/// Task 8, Step 5/6: `gait-min manual print` writes Markdown to stdout only
/// and creates no files (no `skills/`, no `gait-manual/`, no gait-dir).
#[test]
fn min_manual_print_writes_stdout_only() {
    let exe = env!("CARGO_BIN_EXE_gait-min");
    let dir = tempfile::TempDir::new().unwrap();
    let output = std::process::Command::new(exe)
        .args(["manual", "print"])
        .current_dir(dir.path())
        .output()
        .expect("gait-min manual print should run");

    assert!(
        output.status.success(),
        "gait-min manual print failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("# Gait Manual"));
    assert!(stdout.contains("manual-source-hash: "));

    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.is_empty(),
        "gait-min manual print must create no files, found: {:?}",
        entries.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}
