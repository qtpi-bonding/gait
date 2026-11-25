//! Guard: every scope check (a `.starts_with(` near a `.readable` or
//! `.writable` mention) must live in a file that either calls the shared
//! lexical cleaner `crate::sandbox::clean_target` (i.e. contains the text
//! `clean_target(`), or is listed in the explicit ALLOW_LIST with a reason.
//! One door for path validation: either the lexical cleaner, or an audited,
//! justified exception.

use std::fs;
use std::path::{Path, PathBuf};

/// Repo-relative path suffix, one-line reason.
const ALLOW_LIST: &[(&str, &str)] = &[(
    "src/tools/shell/mod.rs",
    "cd target is std::fs::canonicalize'd (physical resolution; a nonexistent path \
     errors out) before the scope check, stricter than a lexical clean",
)];

fn scan(dir: &Path, out: &mut Vec<(PathBuf, usize)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            scan(&p, out);
            continue;
        }
        if p.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        // Skip this guard's own source.
        if p.file_name().and_then(|n| n.to_str()) == Some("scope_check_one_door.rs") {
            continue;
        }
        let text = fs::read_to_string(&p).unwrap();
        // Test modules sit last in these files; stop at the first bare
        // `#[cfg(test)]` line.
        let text = match text.find("\n#[cfg(test)]\n") {
            Some(i) => &text[..i + 1],
            None => &text[..],
        };
        for ln in scope_checks(text) {
            out.push((p.clone(), ln));
        }
    }
}

/// Line numbers (1-based) of `.starts_with(` calls that occur within the same
/// line or the next 4 lines after a `.readable` / `.writable` mention.
/// Comment text after `//` is ignored.
fn scope_checks(path_text: &str) -> Vec<usize> {
    let code: Vec<&str> = path_text
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect();
    let mut hits = Vec::new();
    for (i, line) in code.iter().enumerate() {
        let mentions_scope = line.contains(".readable") || line.contains(".writable");
        if !mentions_scope {
            continue;
        }
        for (j, l) in code.iter().enumerate().skip(i).take(5) {
            if l.contains(".starts_with(") {
                hits.push(j + 1);
            }
        }
    }
    hits.sort_unstable();
    hits.dedup();
    hits
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_scopes() -> Vec<(PathBuf, usize)> {
    let root = root();
    let mut out = Vec::new();
    for c in [
        "src",
        "crates/gait-cli/src",
        "crates/gait-repl/src",
        "crates/gait-sandbox/src",
        "crates/gait-ops/src",
        "crates/gait-acp/src",
    ] {
        scan(&root.join(c), &mut out);
    }
    out
}

#[test]
fn scope_checks_have_one_door() {
    let mut leaks = String::new();
    for (p, ln) in collect_scopes() {
        let rel = p.strip_prefix(root()).unwrap_or(&p).to_string_lossy();
        let text = fs::read_to_string(&p).unwrap();
        let covered = text.contains("clean_target(")
            || ALLOW_LIST.iter().any(|(suffix, _)| rel.ends_with(suffix));
        if !covered {
            let line = text.lines().nth(ln - 1).unwrap_or("").trim().to_string();
            leaks.push_str(&format!("{rel}:{ln}: {line}\n"));
        }
    }
    assert!(
        leaks.trim().is_empty(),
        "scope checks outside the one door (clean_target or ALLOW_LIST):\n{leaks}"
    );
}

#[test]
fn allow_list_entries_still_match_a_scope_check() {
    let root = root();
    for (suffix, reason) in ALLOW_LIST {
        let p = root.join(suffix);
        let text = fs::read_to_string(&p)
            .unwrap_or_else(|e| panic!("allow-list file {suffix} unreadable: {e}"));
        assert!(
            !scope_checks(&text).is_empty(),
            "stale allow-list entry {suffix} ({reason}): no scope check found there anymore"
        );
    }
}
