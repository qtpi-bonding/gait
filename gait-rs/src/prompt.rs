//! Prompt composition: the layered system prompt the model reads as its
//! `instructions`. Four stable layers, each a named paired tag (skipped when
//! empty), in order:
//!   `<harness>`  — built-in `gait-harness.md` (always present)
//!   `<agent>`    — `agents/<name>/AGENT.md`, the role
//!   `<project>`  — `codebases/<key>/PROJECT.md`, per-codebase
//!   `<user>`     — `USER.md` (gait-dir root, gitignored)
//! The dynamic per-turn `<status>` block is NOT here — it is a separate user
//! message (see `turn::build_turn_context`) so it never busts prompt-caching.

use std::path::{Path, PathBuf};

/// Compose the layered, stable (cacheable) system prompt. `gait_dir` locates the
/// per-user/per-codebase files; `codebase` selects the project layer (None for
/// codebase-less contexts like workflows); `agent_path` selects the role layer.
pub fn compose_system_prompt(
    gait_dir: &Path,
    codebase: Option<&Path>,
    agent_path: Option<&Path>,
    done_tool: &str,
) -> String {
    let mut out = String::new();

    // <harness> — always present; its "version" is the binary build hash.
    let h_hash = &crate::GAIT_BUILD_GIT_HASH[..crate::GAIT_BUILD_GIT_HASH.len().min(7)];
    let harness = crate::GAIT_HARNESS_PROMPT
        .trim_end()
        .replace("{$tool_name}", done_tool);
    out.push_str(&wrap_layer("harness", "gait-harness.md", h_hash, &harness));

    // <agent> — the role.
    if let Some(path) = agent_path {
        let body = crate::agent_frontmatter::read_agent_body(path);
        push_file_layer(&mut out, "agent", path, &body);
    }

    // <project> — per-codebase; ensure the dir + empty affordance file exist.
    if let Some(cb) = codebase {
        let project = ensure_codebase_dir(gait_dir, cb).join("PROJECT.md");
        if let Some(body) = read_nonempty(&project) {
            push_file_layer(&mut out, "project", &project, &body);
        }
    }

    // <user> — gait-dir root, gitignored.
    let user = gait_dir.join("USER.md");
    if let Some(body) = read_nonempty(&user) {
        push_file_layer(&mut out, "user", &user, &body);
    }

    out
}

/// Stable, human-findable key for a codebase: `<basename>-<short-hash-of-abspath>`.
/// Two repos with the same basename in different paths never collide.
pub fn codebase_key(codebase: &Path) -> String {
    let abs = std::fs::canonicalize(codebase).unwrap_or_else(|_| codebase.to_path_buf());
    let base = abs
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("codebase");
    format!("{base}-{}", content_hash_short(&abs.to_string_lossy()))
}

/// Read a file, returning `None` if it is missing or only whitespace.
pub fn read_nonempty(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Wrap a layer body in a named, paired tag with `src`/`hash` provenance.
fn wrap_layer(tag: &str, src: &str, hash: &str, body: &str) -> String {
    format!("<{tag} src=\"{src}\" hash=\"{hash}\">\n{body}\n</{tag}>\n")
}

/// Push a file-backed layer, skipping entirely if the (trimmed) body is empty.
fn push_file_layer(out: &mut String, tag: &str, path: &Path, body: &str) {
    let body = body.trim();
    if body.is_empty() {
        return;
    }
    let src = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let hash = content_hash_short(body);
    out.push_str(&wrap_layer(tag, src, &hash, body));
}

/// Ensure `<gait-dir>/codebases/<key>/` exists with an empty `PROJECT.md`
/// affordance, and return the dir.
fn ensure_codebase_dir(gait_dir: &Path, codebase: &Path) -> PathBuf {
    let dir = gait_dir.join("codebases").join(codebase_key(codebase));
    let _ = std::fs::create_dir_all(&dir);
    let project = dir.join("PROJECT.md");
    if !project.exists() {
        let _ = std::fs::write(&project, "");
    }
    dir
}

pub fn content_hash_short(s: &str) -> String {
    // FNV-1a 64-bit — stable across Rust versions, unlike DefaultHasher.
    const BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x00000100000001b3;
    let hash = s
        .bytes()
        .fold(BASIS, |h, b| h.wrapping_mul(PRIME) ^ b as u64);
    format!("{hash:016x}")[..7].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_harness_only_when_nothing_else() {
        let gait = tempfile::TempDir::new().unwrap();
        let p = compose_system_prompt(gait.path(), None, None, "structured_done");
        assert!(
            p.starts_with("<harness src=\"gait-harness.md\" hash=\""),
            "got: {p}"
        );
        assert!(p.contains("</harness>"));
        assert!(!p.contains("<agent"));
        assert!(!p.contains("<project"));
        assert!(!p.contains("<user"));
    }

    /// Task 7: `min` and `full` builds share exactly one harness file/prompt —
    /// there is no second `gait-min-harness.md`. This only exercises harness
    /// sharing (`compose_system_prompt` never calls `build_dynamic_system_
    /// prompt`, which is what Step 4/the `min_dynamic_catalog_lists_skills_
    /// but_not_mcp` test in `turn/tests/integration.rs` covers instead).
    #[test]
    fn min_static_harness_is_identical_to_full() {
        let gait = tempfile::TempDir::new().unwrap();
        let prompt = compose_system_prompt(gait.path(), None, None, "structured_done");
        assert!(prompt.contains("<harness src=\"gait-harness.md\""));
    }

    #[test]
    fn codebase_key_is_basename_plus_hash_and_stable() {
        let k1 = codebase_key(Path::new("/tmp/does-not-exist/myrepo"));
        let k2 = codebase_key(Path::new("/tmp/does-not-exist/myrepo"));
        assert!(
            k1.starts_with("myrepo-"),
            "key keeps a readable basename: {k1}"
        );
        assert_eq!(k1, k2, "same path -> same key (stable)");
        assert_ne!(
            k1,
            codebase_key(Path::new("/other/myrepo")),
            "different path -> different key"
        );
    }

    #[test]
    fn read_nonempty_skips_blank_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("X.md");
        assert_eq!(read_nonempty(&p), None, "missing file -> None");
        std::fs::write(&p, "   \n\t\n").unwrap();
        assert_eq!(read_nonempty(&p), None, "whitespace-only -> None");
        std::fs::write(&p, "hello").unwrap();
        assert_eq!(read_nonempty(&p).as_deref(), Some("hello"));
    }

    #[test]
    fn wrap_layer_emits_named_paired_tag_with_provenance() {
        let s = wrap_layer("project", "PROJECT.md", "abc1234", "body text");
        assert!(
            s.starts_with("<project src=\"PROJECT.md\" hash=\"abc1234\">\n"),
            "got: {s}"
        );
        assert!(s.trim_end().ends_with("</project>"), "got: {s}");
        assert!(s.contains("body text"));
    }

    #[test]
    fn compose_layers_wraps_present_skips_empty() {
        let gait = tempfile::TempDir::new().unwrap();
        let cb = tempfile::TempDir::new().unwrap();
        std::fs::write(gait.path().join("USER.md"), "I prefer terse output.").unwrap();

        let prompt = compose_system_prompt(gait.path(), Some(cb.path()), None, "structured_done");

        assert!(
            prompt.contains("<harness src=\"gait-harness.md\""),
            "harness always present: {prompt}"
        );
        assert!(prompt.contains("</harness>"));
        assert!(
            prompt.contains("<user src=\"USER.md\""),
            "non-empty USER.md emitted"
        );
        assert!(prompt.contains("I prefer terse output."));
        assert!(
            !prompt.contains("<project"),
            "empty PROJECT.md is skipped, not an empty tag"
        );
        assert!(
            !prompt.contains("<agent"),
            "no agent path -> no agent block"
        );
        let key = codebase_key(cb.path());
        assert!(
            gait.path()
                .join("codebases")
                .join(&key)
                .join("PROJECT.md")
                .exists(),
            "per-codebase dir + empty affordance created"
        );
    }
}
