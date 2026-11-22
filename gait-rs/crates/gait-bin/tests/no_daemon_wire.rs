//! The wire and telemetry names say "process", never "daemon". The only
//! "daemon" spellings allowed are "daemonless" (no resident server), the macOS
//! LaunchDaemons directory, the name structmuxd, and the legacy registry file
//! name `daemons.jsonl` that the one-time migration reads.
use std::path::{Path, PathBuf};

const ALLOWED: [&str; 5] = [
    "daemonless",
    "LaunchDaemons",
    "structmuxd",
    "com.apple.cfprefsd.daemon",
    // The legacy registry file the one-time migration still reads.
    "daemons.jsonl",
];
const SKIP_DIRS: [&str; 4] = ["target", ".git", "node_modules", "vendor"];
// `retired.rs` is the one place that names the retired event kinds, so that
// logs holding them fail with a clear error.
const SKIP_FILES: [&str; 3] = ["Cargo.lock", "no_daemon_wire.rs", "retired.rs"];

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                collect(&path, out);
            }
        } else if !SKIP_FILES.contains(&name.as_str()) {
            out.push(path);
        }
    }
}

fn scrub(line: &str) -> String {
    let mut line = line.to_lowercase();
    for allowed in ALLOWED {
        line = line.replace(&allowed.to_lowercase(), "");
    }
    line
}

#[test]
fn no_daemon_spelling_remains_in_code_protos_docs_or_metadata() {
    let rs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let repo = rs.join("..");
    let mut files = Vec::new();
    collect(&rs, &mut files);
    for dir in ["proto", "docs", "metadata"] {
        collect(&repo.join(dir), &mut files);
    }
    let mut hits = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue; // binary
        };
        for (index, line) in text.lines().enumerate() {
            if scrub(line).contains("daemon") {
                hits.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "{} leftover daemon spellings:\n{}",
        hits.len(),
        hits.iter().take(50).cloned().collect::<Vec<_>>().join("\n")
    );
}
