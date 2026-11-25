//! Guard against silently-uncompiled test files.
//!
//! This crate has `autotests = false`, so a `tests/*.rs` file only gets built
//! if it is declared as a `mod` in tests/main.rs or listed as a `[[test]]`
//! target in Cargo.toml. A file that is neither never compiles and its tests
//! never run. This guard fails when that happens (or when the wiring is
//! otherwise inconsistent).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Collect the values of `path = "..."` lines inside `[[test]]` sections of a
/// Cargo.toml. A section starts at a line equal to `[[test]]` (after trim) and
/// ends at the next line starting with `[`.
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
        if let Some(rest) = trimmed.strip_prefix("path")
            && let Some(value) = rest.trim().strip_prefix('=')
        {
            let value = value.trim().trim_matches('"');
            paths.push(value.to_string());
        }
    }
    paths
}

/// Collect module names from `mod name;` / `pub mod name;` lines of tests/main.rs.
fn declared_mods(main_rs: &str) -> BTreeSet<String> {
    let mut mods = BTreeSet::new();
    for line in main_rs.lines() {
        let trimmed = line.trim();
        let body = trimmed
            .strip_prefix("pub mod ")
            .or_else(|| trimmed.strip_prefix("mod "));
        if let Some(body) = body
            && let Some(name) = body.trim().strip_suffix(';')
        {
            mods.insert(name.trim().to_string());
        }
    }
    mods
}

/// Names of `*.rs` files directly under `tests/`. Subdirectories (e.g.
/// `common/`, `fixtures/`) are module or data directories, not test programs,
/// so they are exempt from the wiring checks.
fn test_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return files,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.ends_with(".rs")
        {
            files.push(name.to_string());
        }
    }
    files.sort();
    files
}

#[test]
fn every_test_file_is_compiled_exactly_once() {
    let crate_dir = env!("CARGO_MANIFEST_DIR");
    let tests_dir = Path::new(crate_dir).join("tests");
    let manifest_path = Path::new(crate_dir).join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path).expect("read Cargo.toml");
    let main_rs = fs::read_to_string(tests_dir.join("main.rs")).expect("read tests/main.rs");

    let paths = listed_test_paths(&manifest);
    let mods = declared_mods(&main_rs);
    let files = test_files(&tests_dir);

    let mut problems: Vec<String> = Vec::new();

    // (a) Every .rs file directly under tests/ (other than main.rs) must be
    // wired in somewhere.
    for file in &files {
        if file == "main.rs" {
            continue;
        }
        let stem = file.trim_end_matches(".rs");
        let file_path = format!("tests/{}", file);
        let is_mod = mods.contains(stem);
        let is_listed = paths.iter().any(|p| p == &file_path);
        if !is_mod && !is_listed {
            problems.push(format!(
                "tests/{file} is neither `mod {stem};` in tests/main.rs nor a \
                 [[test]] target in Cargo.toml, so it never compiles: add the \
                 mod line, or a [[test]] entry with a one-line reason comment"
            ));
        }
    }

    // (b) Every declared mod must have a backing file.
    for name in &mods {
        let has_file = tests_dir.join(format!("{name}.rs")).is_file()
            || tests_dir.join(name).join("mod.rs").is_file();
        if !has_file {
            problems.push(format!(
                "tests/main.rs declares `mod {name};` but tests/{name}.rs does \
                 not exist: remove the line or add the file"
            ));
        }
    }

    // (c) Every listed [[test]] path must exist.
    for path in &paths {
        if !Path::new(crate_dir).join(path).is_file() {
            problems.push(format!(
                "[[test]] path {path} in Cargo.toml has no file: remove the \
                 entry or add the file"
            ));
        }
    }

    // (d) A file must not be both a mod and a [[test]] target (it would be
    // compiled twice).
    for file in &files {
        let stem = file.trim_end_matches(".rs");
        let file_path = format!("tests/{}", file);
        if mods.contains(stem) && paths.iter().any(|p| p == &file_path) {
            problems.push(format!(
                "tests/{file} is both declared as `mod {stem};` in \
                 tests/main.rs and listed as [[test]] path {file_path} in \
                 Cargo.toml, so it would be compiled twice: keep exactly one"
            ));
        }
    }

    // (e) The merged program must stay the merged program.
    if !manifest
        .lines()
        .any(|line| line.trim() == "autotests = false")
    {
        problems.push(
            "Cargo.toml must contain `autotests = false`: restore that line, \
             otherwise stray tests/*.rs files are picked up implicitly"
                .to_string(),
        );
    }
    if !paths.iter().any(|p| p == "tests/main.rs") {
        problems.push(
            "Cargo.toml must list a [[test]] target with path = \
             \"tests/main.rs\": restore that entry so the merged test program \
             is built"
                .to_string(),
        );
    }

    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}
