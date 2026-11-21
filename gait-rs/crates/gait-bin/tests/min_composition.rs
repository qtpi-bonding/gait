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

use gait_bin::manual::ManualFlavor;
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
    assert!(
        help.contains("the REPL") || help.contains("-i"),
        "the turn is described: {help}"
    );
    assert!(help.contains("init"), "missing init: {help}");
    assert!(help.contains("sessions"), "missing sessions: {help}");
    assert!(help.contains("config"), "missing config: {help}");
    assert!(help.contains("auth"), "missing auth: {help}");
    assert!(help.contains("inference"), "missing inference: {help}");
    assert!(help.contains("models"), "missing models: {help}");

    // register/enable/disable/load/unload are intentionally still present —
    // they stay unconditional in gait-cli (skill/plugin admin, not
    // workflow- or local-specific).
    for still_present in [
        "register",
        "unregister",
        "enable",
        "disable",
        "load",
        "unload",
    ] {
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
fn min_manual_has_no_install() {
    let exe = env!("CARGO_BIN_EXE_gait-min");
    let output = std::process::Command::new(exe)
        .args(["manual", "--help"])
        .output()
        .expect("gait-min manual --help should run");
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("--json"),
        "expected the manual op's flags: {help}"
    );
    assert!(
        !help.contains("install"),
        "gait-min manual --help must not expose `install`: {help}"
    );
    let install = std::process::Command::new(exe)
        .args(["manual", "install"])
        .output()
        .unwrap();
    assert_eq!(
        install.status.code(),
        Some(2),
        "install is not a gait-min command"
    );
}

/// Renders the Min manual from the Min registry. Confirms `init`, `/new` and
/// sandbox docs are present, and that the full-only "Workflows and
/// extensions" guide (and anything it alone would pull in) is absent.
#[test]
fn min_manual_omits_workflows_and_extensions() {
    let registry = gait_bin::ops::min_registry();
    let rendered =
        gait_bin::manual::render(&registry, ManualFlavor::Min, MIN_REPL_COMMANDS).unwrap();
    assert!(rendered.contains("### gait-min init"));
    assert!(rendered.contains("`/new`"));
    assert!(rendered.contains("`sandbox`"));
    assert!(!rendered.contains("Workflows and extensions"));
    assert!(!rendered.contains("gait-min workflow"));
    assert!(!rendered.contains("/workflow"));

    // Skills are available in both builds: the ungated Skills guide must
    // still render in the Min manual.
    assert!(rendered.contains("### Skills"));
    assert!(rendered.contains("`/skill"));
}

/// `gait-min manual` writes the manual to stdout only and creates no files;
/// `manual --json` has the descriptors but no output schemas (gait-min is
/// built without the `schema` feature).
#[test]
fn min_manual_writes_stdout_only() {
    let exe = env!("CARGO_BIN_EXE_gait-min");
    let dir = tempfile::TempDir::new().unwrap();
    let output = std::process::Command::new(exe)
        .arg("manual")
        .current_dir(dir.path())
        .output()
        .expect("gait-min manual should run");
    assert!(
        output.status.success(),
        "gait-min manual failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("# gait-min"), "{stdout}");

    let json = std::process::Command::new(exe)
        .args(["manual", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let ops = value["ops"].as_array().unwrap();
    assert!(ops.iter().any(|op| op["name"] == "init"));
    assert!(
        ops.iter().all(|op| op.get("outputSchema").is_none()),
        "no output schemas in gait-min"
    );

    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.is_empty(),
        "gait-min manual must create no files, found: {:?}",
        entries.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}
