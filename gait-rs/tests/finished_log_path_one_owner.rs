//! Guard: the finished-session log path function lives in exactly one place
//! (`crates/gait-state/src/session/log/mod.rs`) and is named so its purpose
//! is obvious: `persisted_log_path`. The live log path lives elsewhere
//! (`crates/gait-state/src/session/live.rs`, `session::live::log_path`).
//!
//! Test 1 pins the owner's function name: it must be `persisted_log_path`
//! and must no longer be `log_path`. Test 2 scans all production source and
//! fails if any file other than the owner mentions the finished-session path
//! function; only the live module may mention it, and only in a doc comment.

use std::fs;
use std::path::{Path, PathBuf};

/// This guard's own file name (skipped during the scan).
const SELF: &str = "finished_log_path_one_owner.rs";

/// The one owner of the finished-session log path, repo-relative.
const OWNER: &str = "crates/gait-state/src/session/log/mod.rs";

/// The live module, allowed to mention the name only in comments.
const LIVE: &str = "crates/gait-state/src/session/live.rs";

/// Needles built via `concat!` so this file does not match itself.
fn old_name_needle() -> String {
    concat!("pub fn log_", "path(").to_string()
}
fn new_name_needle() -> String {
    concat!("pub fn persisted_", "log_path(").to_string()
}
fn call_needle() -> String {
    concat!("log::log_", "path(").to_string()
}
fn bare_name_needle() -> String {
    concat!("persisted_", "log_path").to_string()
}

/// Production part of a source file: everything before the first
/// `#[cfg(test)]` line that is directly followed by a test module
/// declaration (`mod ...`, optionally `pub` / `pub(crate)`). A bare
/// `#[cfg(test)]` attached to a single item (not a module) does not end the
/// production part.
fn production_part(text: &str) -> &str {
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        if let Some(next) = lines.get(i + 1) {
            let next = next.trim_start();
            if next.starts_with("mod ")
                || next.starts_with("pub mod ")
                || next.starts_with("pub(crate) mod ")
            {
                let offset: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
                return &text[..offset];
            }
        }
    }
    text
}

fn scan(dir: &Path, root: &Path, out: &mut Vec<String>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            // Nested test directories (e.g. src/turn/tests/) are not
            // production code.
            if p.file_name().and_then(|n| n.to_str()) == Some("tests") {
                continue;
            }
            scan(&p, root, out);
            continue;
        }
        if p.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        // Skip sibling test modules (tests.rs) and this guard itself.
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "tests.rs" || name == SELF {
            continue;
        }
        let rel = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .to_string();
        if rel == OWNER {
            continue; // the owner defines the function
        }
        let text = fs::read_to_string(&p).unwrap();
        let prod = production_part(&text);
        let ignore_comments = rel == LIVE; // doc comment only is allowed there
        for (i, line) in prod.lines().enumerate() {
            if ignore_comments && line.trim_start().starts_with("//") {
                continue;
            }
            let code = line.split("//").next().unwrap_or("");
            let offender = code.contains(&call_needle()) || code.contains(&bare_name_needle());
            if offender {
                out.push(format!("{}:{}: {}", rel, i + 1, line.trim()));
            }
        }
    }
}

fn hits(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    scan(&root.join("src"), root, &mut out);
    let Ok(rd) = fs::read_dir(root.join("crates")) else {
        return out;
    };
    for e in rd {
        let d = e.unwrap().path();
        if d.is_dir() {
            scan(&d.join("src"), root, &mut out);
        }
    }
    out
}

#[test]
fn finished_log_path_has_a_name_that_says_so() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let text = fs::read_to_string(root.join(OWNER))
        .unwrap_or_else(|e| panic!("owner module {OWNER} unreadable: {e}"));
    assert!(
        text.contains(&new_name_needle()),
        "the finished-session log path function must be named \
         `persisted_log_path` in {OWNER}; rename it and update this guard \
         deliberately"
    );
    assert!(
        !text.contains(&old_name_needle()),
        "{OWNER} still defines the ambiguous `pub fn log_path(`; the \
         finished-session function must be `persisted_log_path` so callers \
         needing the LIVE log use `session::live::log_path` instead"
    );
}

#[test]
fn production_code_never_calls_the_finished_session_path() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let leaks = hits(&root);
    assert!(
        leaks.is_empty(),
        "only {OWNER} may define/use the finished-session log path \
         (`persisted_log_path` / `log::log_path(`); {LIVE} is allowed for \
         comments only. For the live log use `session::live::log_path`. \
         Found offenders:\n{}",
        leaks.join("\n")
    );
}
