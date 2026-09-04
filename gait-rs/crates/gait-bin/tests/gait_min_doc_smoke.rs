//! Task 9, Step 1: documentation smoke test.
//!
//! Extracts every fenced `cargo build` command from `docs/gait-min.md`,
//! asserts they are exactly the two documented full/Min build commands (so
//! the doc can't silently drift from the real composition), then actually
//! runs both — proving the copy-pasteable instructions in the doc work,
//! not just that they parse.
//!
//! Does not require any particular feature set to run itself (it only
//! shells out to `cargo`), so it runs under both `full` and `min` test
//! invocations.

use std::path::Path;
use std::process::Command;

fn extract_cargo_build_commands(markdown: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut in_bash_fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```bash") {
            in_bash_fence = true;
            continue;
        }
        if in_bash_fence && trimmed.starts_with("```") {
            in_bash_fence = false;
            continue;
        }
        if in_bash_fence && trimmed.starts_with("cargo build") {
            commands.push(trimmed.to_string());
        }
    }
    commands
}

#[test]
fn gait_min_doc_build_commands_are_exact_and_work() {
    // CARGO_MANIFEST_DIR = <workspace>/crates/gait-bin
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root should resolve");
    let doc_path = workspace_root.join("docs/gait-min.md");
    let markdown = std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", doc_path.display()));

    let commands = extract_cargo_build_commands(&markdown);
    let expected = vec![
        "cargo build -p gait-bin --bin gait".to_string(),
        "cargo build -p gait-bin --no-default-features --features min --bin gait-min".to_string(),
    ];
    assert_eq!(
        commands, expected,
        "docs/gait-min.md's fenced `cargo build` commands must be exactly the \
         two documented full/Min build commands, in order"
    );

    for cmd in &expected {
        let mut parts = cmd.split_whitespace();
        let bin = parts.next().expect("non-empty command");
        let args: Vec<&str> = parts.collect();
        let output = Command::new(bin)
            .args(&args)
            .current_dir(&workspace_root)
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn `{cmd}`: {e}"));
        assert!(
            output.status.success(),
            "`{cmd}` failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
