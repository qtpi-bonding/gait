//! Config verbs: `set` / `get` — the jsonc-safe config mutators shared by
//! the CLI (`gait config set|get`) and the REPL.
//!
//! # Why this exists
//!
//! The original `/model` inline code in `crates/gait-repl/src/repl/driver.rs:311-335`
//! called `serde_json::from_str(&raw).unwrap_or(json!({}))` on the raw
//! `gait.jsonc` contents without first stripping jsonc comments. On any
//! config with `//` comments — or any other parse error — the `unwrap_or`
//! silently wiped the entire file to `{}`, losing every other setting
//! (allow, deny, tools, sandbox, hooks, …). This module fixed that bug as
//! the relocation to the ops layer; the comment-safe primitive is now
//! available to any caller via `set` / `get`.
//!
//! # How it works
//!
//! 1. Read `<gait_dir>/gait.jsonc` (missing file → start from `{}`).
//! 2. Strip `//` and `/* */` comments via
//!    `gait_state::config::strip_jsonc_comments` — the same private helper
//!    `extension.rs` uses. This is the bug fix: a config that contains
//!    comments must not be wiped.
//! 3. Parse the stripped text as a `serde_json::Value::Object`. If the
//!    parse still fails (malformed JSON beyond comments), return
//!    `OpError::Conflict` instead of silently dropping the file.
//! 4. Set/replace the named key (or read it back, for `get`).
//! 5. Pretty-serialize and write back, preserving all other keys.
use crate::OpError;
use std::path::Path;

/// Set `<gait_dir>/gait.jsonc`'s `key` field to `value`, preserving every
/// other key. Comment-safe (see module doc comment).
pub fn set(gait_dir: &Path, key: &str, value: serde_json::Value) -> anyhow::Result<()> {
    let path = gait_dir.join("gait.jsonc");
    let mut val = read_config_object(&path)?;
    val.insert(key.to_string(), value);
    write_config_object(&path, val)
}

/// Read `<gait_dir>/gait.jsonc`'s `key` field, or `None` if the key (or the
/// whole file) is absent. Comment-safe.
pub fn get(gait_dir: &Path, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
    let path = gait_dir.join("gait.jsonc");
    let val = read_config_object(&path)?;
    Ok(val.get(key).cloned())
}

/// Resolve the effective `GaitConfig` for `gait_dir` (Defaults < gait.jsonc,
/// via `load_config`) plus disk-sourced skills/mcp/plugins, and serialize it
/// as JSON for the shared CLI, REPL, and ACP inspection operation.
///
/// This intentionally does NOT build a live `Profile`/`ResolvedProfile`:
/// that requires network provider construction and MCP handshakes, while this
/// operation is introspection rather than a dry run.
pub fn resolve(gait_dir: &Path, codebase: &Path) -> anyhow::Result<serde_json::Value> {
    let _ = codebase;
    let config = gait_state::config::load_config(gait_dir);
    let skills = gait_state::skills::scan_skills(gait_dir);
    let mcp_servers = gait_state::mcp::scan_mcp_servers(gait_dir);
    let mut value = serde_json::to_value(&config)
        .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("config serialize error: {e}"))))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| coded_error::into_anyhow(OpError::Conflict("resolved config is not a JSON object".to_string())))?;
    obj.insert("skills".to_string(), serde_json::to_value(&skills).unwrap());
    obj.insert(
        "mcp_servers".to_string(),
        serde_json::to_value(&mcp_servers).unwrap(),
    );
    Ok(value)
}

// -----------------------------------------------------------------------------
// Private helpers — the jsonc-safe read/write pair. Mirrors the layout of
// `extension.rs` (private helpers sit at the bottom alongside the op that
// uses them). Walking the same path as `read_config_as_value` in
// extension.rs is intentional: both ops need the exact same comment-strip
// + lenient-parse behavior.
// -----------------------------------------------------------------------------

/// Read `gait.jsonc` as a JSON object. Missing file → empty object.
/// Comments are stripped before parsing. A present-but-malformed file is a
/// hard error (caller decides whether to render the conflict or fall back)
/// — this is the bug-fix flip: the pre-fix code dropped the file on any
/// parse error and silently produced an empty object.
fn read_config_object(path: &Path) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".into());
    let stripped = gait_state::config::strip_jsonc_comments(&raw);
    let val: serde_json::Value = serde_json::from_str(&stripped).map_err(|e| {
        coded_error::into_anyhow(OpError::Conflict(format!("gait.jsonc parse error in {}: {e}", path.display())))
    })?;
    match val {
        serde_json::Value::Object(m) => Ok(m),
        // The file parsed but is not a JSON object — surface as Conflict.
        // The pre-fix code's `unwrap_or(json!({}))` would have happily
        // wiped the file in this case too; we refuse to do that.
        other => Err(coded_error::into_anyhow(OpError::Conflict(format!(
            "gait.jsonc root must be a JSON object; got {}",
            json_root_kind(&other)
        )))),
    }
}

fn write_config_object(
    path: &Path,
    obj: serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    let pretty = serde_json::to_string_pretty(&serde_json::Value::Object(obj))
        .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("gait.jsonc serialize error: {e}"))))?;
    std::fs::write(path, pretty + "\n")
        .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("write {}: {e}", path.display()))))
}

fn json_root_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

// -----------------------------------------------------------------------------
// tests
//
// (a) Round-trip of an arbitrary key on a plain config — the value gets set;
//     nothing else is disturbed.
// (b) Comments-and-other-keys config — the regression test that proves the
//     pre-fix inline code would have wiped the file (it didn't strip
//     comments, so `from_str` errored and `unwrap_or(json!({}))` produced
//     an empty object, losing allow/deny/tools/sandbox/etc.). `set`
//     preserves them.
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ws() -> TempDir {
        TempDir::new().unwrap()
    }

    fn parse(raw: &str) -> serde_json::Value {
        serde_json::from_str(&gait_state::config::strip_jsonc_comments(raw)).unwrap()
    }

    #[test]
    fn set_creates_config_when_missing() {
        let dir = ws();
        // No gait.jsonc on disk — `set` should bootstrap the file.
        set(dir.path(), "max_iter", serde_json::json!(50)).unwrap();

        let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        let v = parse(&raw);
        assert_eq!(v["max_iter"], 50);
    }

    /// The regression test: a config with comments AND other keys. The
    /// pre-fix inline REPL code did
    /// `serde_json::from_str(&raw).unwrap_or(json!({}))` without stripping
    /// jsonc comments, so the parse failed on the first `//` line, the
    /// `unwrap_or` produced `{}`, and *every other setting in the file
    /// was lost*. `set` strips comments first, so the parse succeeds
    /// and the write round-trips all other keys.
    #[test]
    fn set_preserves_comments_and_other_keys() {
        let dir = ws();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{
  // top-of-file comment: which model drives /sh
  "model": "openai/gpt-4o-mini",
  "allow": ["read", "glob", "grep"],
  // tool deny list
  "deny": ["shell", "write"],
  // multi-line block comment
  //   (// is the only style strip_jsonc_comments understands today — see
  //    gait-types/config. The pre-fix bug only cares about `//` comments.)
  "tools": [
    { "name": "build", "description": "cargo build", "cmd": "cargo build", "cwd": "codebase" }
  ],
  "sandbox": {
    "on": true,
    "readable": ["$CODEBASE"],
    "writable": ["$CODEBASE"],
    "network": false
  },
  "output_schema_retries": 3,
  "shell_timeout_secs": 300,
  "preview": "on-ask"
}
"#,
        )
        .unwrap();

        set(dir.path(), "max_iter", serde_json::json!(250)).unwrap();

        let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        let v = parse(&raw);

        // The new key got set:
        assert_eq!(v["max_iter"], 250);

        // Every other key from the pre-fix file MUST still be present:
        assert_eq!(v["model"], "openai/gpt-4o-mini");
        assert_eq!(v["allow"][0], "read");
        assert_eq!(v["allow"][1], "glob");
        assert_eq!(v["allow"][2], "grep");
        assert_eq!(v["deny"][0], "shell");
        assert_eq!(v["deny"][1], "write");
        assert_eq!(v["tools"][0]["name"], "build");
        assert_eq!(v["tools"][0]["cmd"], "cargo build");
        assert_eq!(v["sandbox"]["on"], true);
        assert_eq!(v["sandbox"]["readable"][0], "$CODEBASE");
        assert_eq!(v["sandbox"]["writable"][0], "$CODEBASE");
        assert_eq!(v["sandbox"]["network"], false);
        assert_eq!(v["output_schema_retries"], 3);
        assert_eq!(v["shell_timeout_secs"], 300);
        assert_eq!(v["preview"], "on-ask");

        // Top-level object still has all 8 pre-existing keys + the new one.
        assert_eq!(
            v.as_object().unwrap().len(),
            9,
            "expected 9 top-level keys (8 preserved + max_iter), got {:?}",
            v.as_object().unwrap().keys().collect::<Vec<_>>()
        );
    }

    /// If the file is present but not valid JSON even after stripping
    /// comments, `set` must surface the parse error — NOT silently
    /// overwrite the file. The pre-fix inline code would have wiped the
    /// file in this case (its `unwrap_or(json!({}))` swallowed the error).
    #[test]
    fn set_errors_on_present_but_malformed_config_without_wiping() {
        let dir = ws();
        let original = "this is not json at all\n";
        std::fs::write(dir.path().join("gait.jsonc"), original).unwrap();

        let err = set(dir.path(), "max_iter", serde_json::json!(50)).unwrap_err();
        assert!(
            matches!(coded_error::downcast_ref::<OpError>(&err), Some(OpError::Conflict(_))),
            "malformed config must surface as Conflict, got {err:?}"
        );

        // File must be unchanged — never clobber on parse failure.
        let after = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        assert_eq!(after, original, "set must not clobber on parse failure");
    }

    /// A non-object root (e.g. a bare array) is a conflict, not a wipe.
    /// Mirrors the malformed-config behavior: surface the issue without
    /// silently dropping the user's settings.
    #[test]
    fn set_errors_on_non_object_root_without_wiping() {
        let dir = ws();
        let original = "[1, 2, 3]\n";
        std::fs::write(dir.path().join("gait.jsonc"), original).unwrap();

        let err = set(dir.path(), "max_iter", serde_json::json!(50)).unwrap_err();
        assert!(
            matches!(coded_error::downcast_ref::<OpError>(&err), Some(OpError::Conflict(_))),
            "non-object root must surface as Conflict, got {err:?}"
        );
        let after = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        assert_eq!(after, original, "set must not clobber on non-object root");
    }

    #[test]
    fn set_writes_an_arbitrary_top_level_key() {
        let dir = ws();
        std::fs::write(dir.path().join("gait.jsonc"), "{}\n").unwrap();
        set(dir.path(), "max_iter", serde_json::json!(50)).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        let v = parse(&raw);
        assert_eq!(v["max_iter"], 50);
    }

    #[test]
    fn get_returns_none_for_a_missing_key() {
        let dir = ws();
        std::fs::write(dir.path().join("gait.jsonc"), "{}\n").unwrap();
        assert_eq!(get(dir.path(), "nope").unwrap(), None);
    }

    #[test]
    fn get_returns_none_when_config_is_missing_entirely() {
        let dir = ws();
        assert_eq!(get(dir.path(), "model").unwrap(), None);
    }

    #[test]
    fn resolve_returns_effective_config_as_json() {
        let dir = ws();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"inference":"anthropic/claude-sonnet-4-6","allow":["read"]}"#,
        )
        .unwrap();
        let resolved = resolve(dir.path(), dir.path()).unwrap();
        assert_eq!(resolved["inference"], "anthropic/claude-sonnet-4-6");
        assert_eq!(resolved["allow"][0], "read");
    }

    #[test]
    fn resolve_reflects_disk_skills_and_mcp() {
        let dir = ws();
        std::fs::create_dir_all(dir.path().join("skills").join("tdd")).unwrap();
        std::fs::write(
            dir.path().join("skills").join("tdd").join("SKILL.md"),
            "---\nname: tdd\ndescription: d\n---\n# body\n",
        )
        .unwrap();
        let resolved = resolve(dir.path(), dir.path()).unwrap();
        let skills = resolved["skills"].as_array().unwrap();
        assert!(skills.iter().any(|s| s["name"] == "tdd"));
    }
}
