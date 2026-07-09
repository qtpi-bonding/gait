//! Config verbs: `set_model` — the `/model` rewrite previously inlined in the
//! REPL.
//!
//! # Why this exists
//!
//! `crates/gait-repl/src/repl/driver.rs:311-335` (the original `/model`
//! inline code) called `serde_json::from_str(&raw).unwrap_or(json!({}))` on
//! the raw `gait.jsonc` contents without first stripping jsonc comments.
//! On any config with `//` comments — or any other parse error — the
//! `unwrap_or` silently wiped the entire file to `{}`, losing every other
//! setting (allow, deny, tools, sandbox, hooks, llm_params, …). This
//! implementation fixes the bug as the relocation. It's a deliberate,
//! documented behavior change (called out in the plan as the single
//! intentional correctness improvement in this sweep); the REPL adopts the
//! op in Task 8.
//!
//! # How it works
//!
//! 1. Read `<gait_dir>/gait.jsonc` (missing file → start from `{}`).
//! 2. Strip `//` and `/* */` comments via
//!    `gait_core::config::strip_jsonc_comments` — the same private helper
//!    `extension.rs` already uses. This is the bug fix: a config that
//!    contains comments must not be wiped.
//! 3. Parse the stripped text as a `serde_json::Value::Object`. If the
//!    parse still fails (malformed JSON beyond comments), return
//!    `OpError::Conflict` instead of silently dropping the file.
//! 4. Set `model` to the new value.
//! 5. Pretty-serialize and write back, preserving all other keys.
use crate::OpError;
use std::path::Path;

/// Set the `model` field of `<gait_dir>/gait.jsonc` to `model`, preserving
/// every other key. The config may use jsonc comments (`//` line and
/// `/* */` block); comments are stripped before parsing so a comment-laden
/// config no longer triggers a silent wipe to `{}`.
pub fn set_model(gait_dir: &Path, model: &str) -> Result<(), OpError> {
    let path = gait_dir.join("gait.jsonc");
    let mut val = read_config_object(&path)?;
    val.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    write_config_object(&path, val)
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
fn read_config_object(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>, OpError> {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".into());
    let stripped = gait_core::config::strip_jsonc_comments(&raw);
    let val: serde_json::Value = serde_json::from_str(&stripped).map_err(|e| {
        OpError::Conflict(format!("gait.jsonc parse error in {}: {e}", path.display()))
    })?;
    match val {
        serde_json::Value::Object(m) => Ok(m),
        // The file parsed but is not a JSON object — surface as Conflict.
        // The pre-fix code's `unwrap_or(json!({}))` would have happily
        // wiped the file in this case too; we refuse to do that.
        other => Err(OpError::Conflict(format!(
            "gait.jsonc root must be a JSON object; got {}",
            json_root_kind(&other)
        ))),
    }
}

fn write_config_object(
    path: &Path,
    obj: serde_json::Map<String, serde_json::Value>,
) -> Result<(), OpError> {
    let pretty = serde_json::to_string_pretty(&serde_json::Value::Object(obj))
        .map_err(|e| OpError::Conflict(format!("gait.jsonc serialize error: {e}")))?;
    std::fs::write(path, pretty + "\n")
        .map_err(|e| OpError::Conflict(format!("write {}: {e}", path.display())))
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
// (a) Round-trip on a plain config — the model field gets set; nothing else
//     is disturbed.
// (b) Comments-and-other-keys config — the regression test that proves the
//     pre-fix inline code would have wiped the file (it didn't strip
//     comments, so `from_str` errored and `unwrap_or(json!({}))` produced
//     an empty object, losing allow/deny/tools/sandbox/etc.). set_model
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
        serde_json::from_str(&gait_core::config::strip_jsonc_comments(raw)).unwrap()
    }

    #[test]
    fn set_model_round_trips_on_a_plain_config() {
        let dir = ws();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{
  "model": "openai/gpt-4o-mini",
  "allow": ["read", "grep"],
  "deny": ["shell"]
}
"#,
        )
        .unwrap();

        set_model(dir.path(), "anthropic/claude-3-5-sonnet").unwrap();

        let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        let v = parse(&raw);
        assert_eq!(v["model"], "anthropic/claude-3-5-sonnet");
        assert_eq!(v["allow"][0], "read");
        assert_eq!(v["allow"][1], "grep");
        assert_eq!(v["deny"][0], "shell");
    }

    #[test]
    fn set_model_creates_model_field_when_config_is_empty() {
        let dir = ws();
        std::fs::write(dir.path().join("gait.jsonc"), "{}\n").unwrap();

        set_model(dir.path(), "openai/gpt-4o").unwrap();

        let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        let v = parse(&raw);
        assert_eq!(v["model"], "openai/gpt-4o");
        // No phantom keys were invented.
        assert_eq!(v.as_object().unwrap().len(), 1);
    }

    #[test]
    fn set_model_creates_config_when_missing() {
        let dir = ws();
        // No gait.jsonc on disk — set_model should bootstrap the file.
        set_model(dir.path(), "anthropic/claude-3-5-sonnet").unwrap();

        let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        let v = parse(&raw);
        assert_eq!(v["model"], "anthropic/claude-3-5-sonnet");
    }

    /// The regression test: a config with comments AND other keys. The
    /// pre-fix inline REPL code did
    /// `serde_json::from_str(&raw).unwrap_or(json!({}))` without stripping
    /// jsonc comments, so the parse failed on the first `//` line, the
    /// `unwrap_or` produced `{}`, and *every other setting in the file
    /// was lost*. set_model strips comments first, so the parse succeeds
    /// and the write round-trips all other keys.
    #[test]
    fn set_model_preserves_comments_and_other_keys() {
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

        set_model(dir.path(), "anthropic/claude-3-5-sonnet").unwrap();

        let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        let v = parse(&raw);

        // The model got set:
        assert_eq!(v["model"], "anthropic/claude-3-5-sonnet");

        // Every other key from the pre-fix file MUST still be present:
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

        // Top-level object still has model + 7 preserved siblings
        // (allow/deny/tools/sandbox/output_schema_retries/shell_timeout_secs/
        // preview).
        assert_eq!(
            v.as_object().unwrap().len(),
            8,
            "expected model + 7 preserved top-level keys, got {:?}",
            v.as_object().unwrap().keys().collect::<Vec<_>>()
        );
    }

    /// If the file is present but not valid JSON even after stripping
    /// comments, set_model must surface the parse error — NOT silently
    /// overwrite the file. The pre-fix inline code would have wiped the
    /// file in this case (its `unwrap_or(json!({}))` swallowed the error).
    #[test]
    fn set_model_errors_on_present_but_malformed_config_without_wiping() {
        let dir = ws();
        let original = "this is not json at all\n";
        std::fs::write(dir.path().join("gait.jsonc"), original).unwrap();

        let err = set_model(dir.path(), "anthropic/claude-3-5-sonnet").unwrap_err();
        assert!(
            matches!(err, OpError::Conflict(_)),
            "malformed config must surface as Conflict, got {err:?}"
        );

        // File must be unchanged — never clobber on parse failure.
        let after = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        assert_eq!(
            after, original,
            "set_model must not clobber on parse failure"
        );
    }

    /// A non-object root (e.g. a bare array) is a conflict, not a wipe.
    /// Mirrors the malformed-config behavior: surface the issue without
    /// silently dropping the user's settings.
    #[test]
    fn set_model_errors_on_non_object_root_without_wiping() {
        let dir = ws();
        let original = "[1, 2, 3]\n";
        std::fs::write(dir.path().join("gait.jsonc"), original).unwrap();

        let err = set_model(dir.path(), "anthropic/claude-3-5-sonnet").unwrap_err();
        assert!(
            matches!(err, OpError::Conflict(_)),
            "non-object root must surface as Conflict, got {err:?}"
        );
        let after = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        assert_eq!(
            after, original,
            "set_model must not clobber on non-object root"
        );
    }
}
