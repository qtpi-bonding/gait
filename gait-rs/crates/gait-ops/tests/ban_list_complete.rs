//! Wall-2 backstop: every `pub async fn run_*` under gait-core's turn/embed modules
//! must be EITHER banned in gait-ops/clippy.toml OR explicitly allowlisted here with
//! a reason — so a new turn entrypoint can't silently bypass the ban, and a
//! non-turn `run_*` doesn't spuriously fail the build.
use std::fs;
use std::path::{Path, PathBuf};

/// `run_*` fns that are NOT model-turn entrypoints (so not banned). Each needs a
/// reason. If a new `run_*` lands, classify it: ban it in clippy.toml, or add it here.
const ALLOWLIST: &[&str] = &[
    // spine-internal tool executor; runs a single tool, not a model turn, and is not
    // reachable from an op in practice. See src/turn/spine/handlers/tool.rs.
    "run_tool_as_spine",
];

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
            out.push(p);
        }
    }
}

#[test]
fn every_run_entrypoint_is_banned_or_allowlisted() {
    let core = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let clippy =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("clippy.toml")).unwrap();

    let mut sources = vec![core.join("src/embed.rs")];
    rs_files(&core.join("src/turn"), &mut sources);

    let mut unclassified = Vec::new();
    for src in sources {
        let text = fs::read_to_string(&src).unwrap_or_default();
        for line in text.lines() {
            if let Some(rest) = line.trim_start().strip_prefix("pub async fn run_") {
                let name = format!("run_{}", rest.split('(').next().unwrap_or("").trim());
                let banned = clippy.contains(&format!("::{name}\""));
                let allowed = ALLOWLIST.contains(&name.as_str());
                if !banned && !allowed {
                    unclassified.push(format!("{} :: {name}", src.display()));
                }
            }
        }
    }
    assert!(
        unclassified.is_empty(),
        "new turn entrypoint(s) — ban in clippy.toml or allowlist with a reason: {unclassified:#?}"
    );
}
