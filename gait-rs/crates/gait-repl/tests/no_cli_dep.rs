//! Guard: gait-repl must not depend on gait-cli.
//!
//! gait-cli contains the high-level CLI subcommand wiring.  gait-repl is a
//! library crate that must stay independent of it so the REPL can be embedded
//! without pulling in the full CLI surface.
use std::fs;

#[test]
fn repl_has_no_gait_cli_dep() {
    let manifest = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    assert!(
        !manifest.contains("gait-cli"),
        "gait-repl/Cargo.toml must not list gait-cli as a dependency:\n{manifest}"
    );
}
