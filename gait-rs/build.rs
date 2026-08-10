use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn main() {
    capture_git_hash();
    verify_manual();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=sandbox");
    }
}

fn verify_manual() {
    println!("cargo:rerun-if-env-changed=GAIT_MANUAL_UPDATE");
    if std::env::var("GAIT_MANUAL_UPDATE").as_deref() == Ok("1") {
        return;
    }
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let skill_path = root.join("gait-manual/SKILL.md");
    let skill = std::fs::read_to_string(&skill_path).unwrap_or_else(|e| {
        panic!(
            "missing generated gait manual at {}: {e}; run `cargo run -p gait-mangen -- target/gait-man gait-manual/SKILL.md`",
            skill_path.display()
        )
    });
    let expected = manual_source_hash(&root);
    let marker = "manual-source-hash: ";
    let actual = skill
        .lines()
        .find_map(|line| line.strip_prefix(marker))
        .unwrap_or("");
    if actual != expected {
        panic!(
            "gait-manual/SKILL.md is stale (expected source hash {expected}, found {actual:?}); run `cargo run -p gait-mangen -- target/gait-man gait-manual/SKILL.md`"
        );
    }
    println!("cargo:rerun-if-changed={}", skill_path.display());
}

fn manual_source_hash(root: &Path) -> String {
    let mut files = vec![root.join("gait-manual/manual.yaml")];
    for relative in [
        "crates/gait-mangen/src",
        "crates/gait-bin/src",
        "crates/gait-cli/src",
        "crates/gait-repl/src/repl",
    ] {
        collect_rust_files(&root.join(relative), &mut files);
    }
    files.sort();

    let mut hasher = Sha256::new();
    for path in files {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(
            std::fs::read(&path)
                .unwrap_or_else(|e| panic!("read manual source {}: {e}", path.display())),
        );
        hasher.update([0]);
        println!("cargo:rerun-if-changed={}", path.display());
    }
    format!("{:x}", hasher.finalize())
}

fn collect_rust_files(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_file() {
        if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path.to_path_buf());
        }
        return;
    }
    for entry in std::fs::read_dir(path)
        .unwrap_or_else(|e| panic!("read manual source directory {}: {e}", path.display()))
        .flatten()
    {
        collect_rust_files(&entry.path(), files);
    }
}

/// Capture git hash at compile time for attribution in the system prompt.
fn capture_git_hash() {
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GAIT_BUILD_GIT_HASH={}", hash);
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/");
}
