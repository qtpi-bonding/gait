//! Guard: the per-tool sandbox override map (`SandboxConfig.tools`, values
//! `ToolAccessOverride` with reads/writes/network/local_bind) must be read in
//! exactly ONE place — the owner `crates/gait-sandbox/src/sandbox.rs`. Any
//! other lookup into the map (`tools.get(`) outside that file, or more than
//! one lookup inside it, means a second reader slipped in and the override
//! policy has multiple enforcement points.
use std::fs;
use std::path::{Path, PathBuf};

const OWNER: &str = "crates/gait-sandbox/src/sandbox.rs";

fn needle() -> String {
    // Built via concat! so the guard does not self-match.
    concat!("tools", ".get(").to_string()
}

fn scan(dir: &Path, needle: &str, out: &mut Vec<String>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            scan(&p, needle, out);
            continue;
        }
        if p.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        // Skip the guard itself.
        if p.file_name().and_then(|n| n.to_str()) == Some("shell_override_one_reader.rs") {
            continue;
        }
        let rel = p
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(&p)
            .to_string_lossy()
            .to_string();
        if rel.starts_with("crates/gait-types") {
            continue; // defines the type, exempt
        }
        for (i, line) in fs::read_to_string(&p).unwrap().lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains(needle) {
                out.push(format!("{}:{}: {}", rel, i + 1, line.trim()));
            }
        }
    }
}

fn crate_src_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(root.join("crates")) else {
        return out;
    };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.push(p.join("src"));
        }
    }
    out
}

fn hits(root: &Path, needle: &str) -> Vec<String> {
    let mut leaks = Vec::new();
    scan(&root.join("src"), needle, &mut leaks);
    for d in crate_src_dirs(root) {
        scan(&d, needle, &mut leaks);
    }
    leaks
}

#[test]
fn tool_override_map_has_one_reader() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let leaks = hits(&root, &needle());
    assert!(
        leaks.iter().all(|l| l.starts_with(OWNER)),
        "SandboxConfig.tools override map must be read only in {OWNER}:\n{}",
        leaks.join("\n")
    );
    let owner_hits: Vec<&String> = leaks.iter().filter(|l| l.starts_with(OWNER)).collect();
    assert_eq!(
        owner_hits.len(),
        1,
        "{OWNER} must contain exactly one `tools.get(` reader, found {}:\n{}",
        owner_hits.len(),
        owner_hits
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn owner_has_exactly_one_reader() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let owner_path = root.join(OWNER);
    let needle = needle();
    let count = fs::read_to_string(&owner_path)
        .unwrap()
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let code = line.split("//").next().unwrap_or("");
            code.contains(&needle)
        })
        .count();
    assert_eq!(
        count, 1,
        "{OWNER} must have exactly one `tools.get(` reader, found {count}"
    );
}
