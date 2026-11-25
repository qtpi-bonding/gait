//! Guard: the once/session unlock grant sets of `ActiveUnlockState` may only
//! be READ by the gait-state module that defines them
//! (`crates/gait-state/src/session/retrace/mod.rs`, home of
//! `ActiveUnlockState::grants`). Any other production file that mentions the
//! state type yet touches `.once` / `.session` directly — or builds an
//! `ActiveUnlockState { ... }` literal outside the defining module — means a
//! second reader slipped in and grant enforcement has multiple doors.
//!
//! A companion test pins the existence of the single reader's signature, so a
//! rename cannot silently empty this guard.

use std::fs;
use std::path::{Path, PathBuf};

/// This guard's own file name (skipped during the scan).
const SELF: &str = "unlock_grants_one_reader.rs";

/// The one allowed reader/definer, repo-relative.
const OWNER: &str = "crates/gait-state/src/session/retrace/mod.rs";

/// Repo-relative path suffix, offending-line substring, one-line reason.
/// Only exact offending lines listed here are exempt.
const ALLOW_LIST: &[(&str, &str, &str)] = &[(
    "src/turn/spine/handlers/tool.rs",
    "session: rec.session.clone()",
    "`rec` is a process-registry record; its `.session` field is the owning \
     parent session id for processes.jsonl, not the unlock grant set. The \
     file's only unlock use is `active_unlock_state(&events).grants(...)`",
)];

/// Needles built via `concat!` so this file does not match itself.
fn type_needle() -> String {
    concat!("ActiveUnlock", "State").to_string()
}
fn fn_needle() -> String {
    concat!("active_unlock_", "state(").to_string()
}
fn literal_needle() -> String {
    concat!("ActiveUnlock", "State {").to_string()
}

/// Substrings that mark a `.once` / `.session` FIELD ACCESS (not a word like
/// "session" in prose): the field name followed by `.` (method/field chain),
/// `)` or `;`.
fn field_needles() -> Vec<String> {
    [
        concat!(".on", "ce."),
        concat!(".on", "ce)"),
        concat!(".on", "ce;"),
        concat!(".sess", "ion."),
        concat!(".sess", "ion)"),
        concat!(".sess", "ion;"),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
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
            continue; // the defining module is the one allowed reader
        }
        let text = fs::read_to_string(&p).unwrap();
        let prod = production_part(&text);
        let mentions_type = prod.contains(&type_needle()) || prod.contains(&fn_needle());
        for (i, line) in prod.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            let offender = (mentions_type && field_needles().iter().any(|n| code.contains(n)))
                || code.contains(&literal_needle());
            if offender
                && ALLOW_LIST
                    .iter()
                    .any(|(suffix, frag, _)| rel.ends_with(suffix) && line.trim().contains(frag))
            {
                continue;
            }
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
fn unlock_grants_have_one_reader() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let leaks = hits(&root);
    assert!(
        leaks.is_empty(),
        "once/session unlock grant sets must be read only in {OWNER} \
         (via ActiveUnlockState::grants); found direct readers/literals:\n{}",
        leaks.join("\n")
    );
}

#[test]
fn defining_module_still_exists() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let text = fs::read_to_string(root.join(OWNER))
        .unwrap_or_else(|e| panic!("owner module {OWNER} unreadable: {e}"));
    // Not concat!: this string is only a signature check, and this file does
    // not scan itself for it.
    let needle = "pub fn grants(&self, path: &std::path::Path, dimension";
    assert!(
        text.contains(needle),
        "the single reader signature `ActiveUnlockState::{needle}` vanished \
         from {OWNER}; if it was renamed, update this guard deliberately"
    );
}
