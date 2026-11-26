//! Workspace-wide guard: every crate's integration tests are either modules of
//! that crate's single `tests/main.rs` program (with `autotests = false` and one
//! `[[test]] name = "tests"`), or deliberate standalone `[[test]]` targets that
//! are explicitly listed in `SEPARATE_TARGETS` with a reason. A `tests/*.rs`
//! file that is neither a `mod` nor a listed target never compiles; this test
//! makes that state impossible to reach silently.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Standalone `[[test]]` targets that are deliberately not modules of their
/// crate's `tests/main.rs`, as (path relative to the gait-rs workspace root,
/// one-line reason). Rule (3) below keeps every manifest's non-main `[[test]]`
/// path exactly aligned with this list.
const SEPARATE_TARGETS: &[(&str, &str)] = &[
    (
        "tests/repl_live_render.rs",
        "sets process-global env via set_var",
    ),
    (
        "tests/integration_workflow.rs",
        "sets process-global env via set_var",
    ),
    (
        "tests/spine_watchdog.rs",
        "sets process-global env via set_var",
    ),
    (
        "tests/telemetry_flush.rs",
        "installs the process-global telemetry subscriber",
    ),
    (
        "tests/telemetry_flush_down.rs",
        "installs the process-global telemetry subscriber",
    ),
    (
        "tests/telemetry_init_failed.rs",
        "installs the process-global telemetry subscriber",
    ),
    (
        "tests/landlock_seal.rs",
        "seals the whole process (ignored tests, run alone)",
    ),
    (
        "tests/seatbelt_seal.rs",
        "seals the whole process (ignored tests, run alone)",
    ),
    (
        "crates/gait-bin/tests/error_record.rs",
        "sets process-global env via set_var",
    ),
    (
        "crates/gait-bin/tests/process_exit_wake.rs",
        "sets process-global env via set_var",
    ),
    (
        "crates/gait-bin/tests/ops_registry.rs",
        "sets process-global env and changes the cwd",
    ),
    (
        "crates/gait-bin/tests/trace_join.rs",
        "uses the process-global telemetry capture",
    ),
    (
        "crates/gait-acp/tests/cancel.rs",
        "sets process-global GAIT_HOME and GAIT_BRAIN_EXE via set_var",
    ),
    (
        "crates/gait-acp/tests/clarify_inform_unlock.rs",
        "sets process-global GAIT_HOME and GAIT_BRAIN_EXE via set_var",
    ),
    (
        "crates/gait-acp/tests/fs_delegate.rs",
        "sets process-global GAIT_HOME and GAIT_BRAIN_EXE via set_var",
    ),
    (
        "crates/gait-acp/tests/permission.rs",
        "sets process-global GAIT_HOME and GAIT_BRAIN_EXE via set_var",
    ),
    (
        "crates/gait-acp/tests/prompt.rs",
        "sets process-global GAIT_HOME and GAIT_BRAIN_EXE via set_var",
    ),
    (
        "crates/gait-acp/tests/prompt_idempotency.rs",
        "sets process-global GAIT_HOME and GAIT_BRAIN_EXE via set_var",
    ),
    (
        "crates/gait-acp/tests/prompt_provenance.rs",
        "sets process-global GAIT_HOME and GAIT_BRAIN_EXE via set_var",
    ),
    (
        "crates/gait-acp/tests/trace.rs",
        "sets process-global GAIT_HOME and GAIT_BRAIN_EXE via set_var",
    ),
    (
        "crates/gait-acp/tests/trace_carrier.rs",
        "sets process-global GAIT_HOME and GAIT_BRAIN_EXE via set_var",
    ),
    (
        "crates/gait-provider/tests/openai_call_span.rs",
        "sets the process-global LMSTUDIO_BASE_URL via set_var",
    ),
    (
        "crates/gait-provider/tests/openai_stream_usage.rs",
        "sets the process-global LMSTUDIO_BASE_URL via set_var",
    ),
    (
        "crates/gait-provider/tests/openai_wire_params.rs",
        "sets the process-global LMSTUDIO_BASE_URL via set_var",
    ),
    (
        "crates/gait-provider/tests/wire_call_spans.rs",
        "sets the process-global LMSTUDIO_BASE_URL via set_var",
    ),
    (
        "crates/gait-repl/tests/repl_drive_e2e.rs",
        "harness = false and required-features",
    ),
    (
        "crates/gait-repl/tests/plain_repl_e2e.rs",
        "required-features",
    ),
    (
        "crates/gait-cli/tests/exec_no_gait_dir.rs",
        "sets process-global env via set_var and reads the cwd",
    ),
    (
        "crates/gait-codemode/tests/signal_interrupt.rs",
        "sends real signals to the process and relies on process-global trap handler state, so it must not share a binary",
    ),
];

/// The quoted strings of the workspace manifest's `members = [ ... ]` list.
fn workspace_members(root_manifest: &str) -> Vec<String> {
    let Some(start) = root_manifest.find("members = [") else {
        return Vec::new();
    };
    let body = &root_manifest[start..];
    let Some(end) = body.find(']') else {
        return Vec::new();
    };
    // Every second quote-separated piece between the outer quotes is a member.
    body[..end]
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// The `path = "..."` values inside `[[test]]` sections of a manifest.
/// A section starts at a trimmed `[[test]]` line and ends at the next line
/// that starts with `[`.
fn listed_test_paths(manifest: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut in_test_section = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed == "[[test]]" {
            in_test_section = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_test_section = false;
            continue;
        }
        if !in_test_section {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("path = \"") {
            if let Some(value) = rest.strip_suffix('"') {
                paths.push(value.to_owned());
            }
        }
    }
    paths
}

/// The module names declared by `mod x;` / `pub mod x;` lines in a `main.rs`.
fn declared_mods(main_rs: &str) -> BTreeSet<String> {
    let mut mods = BTreeSet::new();
    for line in main_rs.lines() {
        let trimmed = line.trim();
        let trimmed = trimmed.strip_prefix("pub ").unwrap_or(trimmed);
        if let Some(rest) = trimmed.strip_prefix("mod ") {
            if let Some(name) = rest.strip_suffix(';') {
                mods.insert(name.trim().to_owned());
            }
        }
    }
    mods
}

/// The `.rs` files directly under a crate's `tests/` directory, sorted.
/// Subdirectories such as `common/` and `fixtures/` are module or data
/// directories, not test files, so they are not listed.
fn test_files(tests_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(tests_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "rs"))
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

#[test]
fn workspace_test_binaries_are_merged_and_listed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root_manifest = fs::read_to_string(root.join("Cargo.toml")).expect("workspace manifest");
    let member_dirs = workspace_members(&root_manifest);

    // Crate dirs to check: the workspace root itself plus each member that has
    // both a Cargo.toml and a tests/ directory.
    let mut crate_dirs: Vec<String> = vec![String::from(".")];
    for member in &member_dirs {
        let dir = root.join(member);
        if dir.join("Cargo.toml").is_file() && dir.join("tests").is_dir() {
            crate_dirs.push(member.clone());
        }
    }

    let mut problems: Vec<String> = Vec::new();

    for crate_dir in &crate_dirs {
        // In messages the root is named "."; on disk it is the root itself.
        let base = if crate_dir == "." {
            root.to_path_buf()
        } else {
            root.join(crate_dir)
        };
        let dir_name = crate_dir.as_str();

        let manifest = fs::read_to_string(base.join("Cargo.toml")).unwrap_or_default();
        let files_all = test_files(&base.join("tests"));
        let files: Vec<&String> = files_all
            .iter()
            .filter(|file| file.as_str() != "main.rs")
            .collect();
        let main_rs = fs::read_to_string(base.join("tests/main.rs")).unwrap_or_default();
        let mods = declared_mods(&main_rs);
        let listed = listed_test_paths(&manifest);

        // (1) A crate with more than one tests/*.rs file, or with a main.rs,
        // must be merged into the single `tests` program.
        let must_be_merged = files.len() > 1 || base.join("tests/main.rs").is_file();
        if must_be_merged {
            let has_autotests_off = manifest.contains("autotests = false");
            let lists_main = listed.iter().any(|path| path == "tests/main.rs");
            if !has_autotests_off || !lists_main {
                problems.push(format!(
                    "{dir_name} has {} tests/*.rs files and is not merged into one program: \
                     add autotests = false and a [[test]] name = \"tests\" path = \"tests/main.rs\" \
                     with a mod per file (or list the file in SEPARATE_TARGETS with a reason)",
                    files.len()
                ));
            }
        }

        // (2) Every non-main file of a merged crate must be a `mod` in
        // main.rs, or a listed [[test]] target named in SEPARATE_TARGETS.
        if must_be_merged {
            for file in &files {
                let stem = file.trim_end_matches(".rs");
                let separate_path = if dir_name == "." {
                    format!("tests/{}", file)
                } else {
                    format!("{}/tests/{}", dir_name, file)
                };
                let is_separate_listed = SEPARATE_TARGETS
                    .iter()
                    .any(|(path, _)| *path == separate_path);
                let is_listed_target = listed.iter().any(|path| path == &format!("tests/{}", file));
                let is_mod = mods.contains(stem);

                if is_mod && (is_separate_listed || is_listed_target) {
                    problems.push(format!(
                        "{}/tests/{} is both a mod in tests/main.rs and a separate \
                         [[test]] target, so it compiles twice",
                        dir_name, file
                    ));
                    continue;
                }
                if !is_mod && !(is_separate_listed && is_listed_target) {
                    problems.push(format!(
                        "{}/tests/{} is neither a mod in tests/main.rs nor a listed \
                         separate target, so it never compiles: add `mod {};` to main.rs, \
                         or a [[test]] entry with a reason comment and a SEPARATE_TARGETS line",
                        dir_name, file, stem
                    ));
                }
            }
        }

        // (3) Every listed path other than tests/main.rs must be in
        // SEPARATE_TARGETS.
        for path in &listed {
            if path == "tests/main.rs" {
                continue;
            }
            let full = if dir_name == "." {
                path.clone()
            } else {
                format!("{}/{}", dir_name, path)
            };
            let in_guard = SEPARATE_TARGETS.iter().any(|(entry, _)| entry == &full);
            if !in_guard {
                problems.push(format!(
                    "{} declares a standalone test target {}: add it to SEPARATE_TARGETS \
                     with a reason, or make it a mod of tests/main.rs",
                    dir_name, full
                ));
            }
        }

        // (4) Every `mod x;` in a main.rs must have tests/x.rs or tests/x/mod.rs.
        for name in &mods {
            let file = base.join("tests").join(format!("{}.rs", name));
            let dir_mod = base.join("tests").join(name).join("mod.rs");
            if !file.is_file() && !dir_mod.is_file() {
                problems.push(format!(
                    "{}/tests/main.rs declares `mod {};` but tests/{}.rs (or tests/{}/mod.rs) \
                     does not exist",
                    dir_name, name, name, name
                ));
            }
        }
    }

    // (5) Every SEPARATE_TARGETS entry: non-empty reason, existing file, and
    // listed as a [[test]] path by its crate.
    for (path, reason) in SEPARATE_TARGETS {
        if reason.trim().is_empty() {
            problems.push(format!("SEPARATE_TARGETS names {} without a reason", path));
        }
        if !root.join(path).is_file() {
            problems.push(format!(
                "SEPARATE_TARGETS names {} but the file does not exist",
                path
            ));
        }
        // The crate is the part before "/tests/", or the root when the path
        // starts with "tests/".
        let path = *path;
        let (crate_dir, crate_rel) = if path.starts_with("tests/") {
            (".", path)
        } else if let Some(idx) = path.find("/tests/") {
            (&path[..idx], &path[idx + 1..])
        } else {
            problems.push(format!(
                "SEPARATE_TARGETS names {} but it is not a <crate>/tests/<file>.rs path",
                path
            ));
            continue;
        };
        let base = if crate_dir == "." {
            root.to_path_buf()
        } else {
            root.join(crate_dir)
        };
        let manifest = fs::read_to_string(base.join("Cargo.toml")).unwrap_or_default();
        let listed = listed_test_paths(&manifest);
        if !listed.iter().any(|entry| entry == crate_rel) {
            problems.push(format!(
                "SEPARATE_TARGETS names {} but its crate does not declare a [[test]] \
                 with path = \"tests/{}\"",
                path, crate_rel
            ));
        }
    }

    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}
