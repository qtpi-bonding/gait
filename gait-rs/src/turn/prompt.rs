//! Prompt-construction helpers used by the streaming turn loop. Owns the
//! `<loadable-catalog>`, `<loadable-state>`, `<permission-status>`, and
//! `<status>` system-prompt fragments plus the `MakeMcpToolDef` shim and
//! the dynamic assembly step that folds hooks into the live system prompt.

use std::path::Path;
use std::sync::Arc;

use crate::config::McpToolDef;
use crate::mcp::McpClient;
use crate::types::{Hook, LoadableEntry, Tool};

/// Render the `<status>` block the model sees at the top of every turn:
/// unix timestamp, codebase HEAD, dirty files, and a terminal-style cwd
/// prompt. Tested directly — keep `pub(crate)` so `turn/tests.rs` can call it.
pub(crate) fn build_turn_context(
    codebase_hash: &str,
    dirty_files: &[String],
    cwd: &Path,
    daemons: &[(String, i32, bool, String)],
) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let mut ctx = format!(
        "<status>\nCurrent time (unix): {}\nCodebase commit: {}\n",
        secs, codebase_hash
    );
    if !dirty_files.is_empty() {
        ctx.push_str("Dirty files:\n");
        for f in dirty_files {
            ctx.push_str(&format!("  {}\n", f));
        }
    }
    if !daemons.is_empty() {
        ctx.push_str("Daemons:\n");
        for (name, pid, alive, log_path) in daemons {
            let status = if *alive { "running" } else { "stopped" };
            ctx.push_str(&format!(
                "  {} (pid {}) — {} — log: {}\n",
                name, pid, status, log_path
            ));
        }
    }
    // Terminal-style prompt: reminds the agent where `shell` runs (sticky cwd).
    ctx.push_str(&format!("{} %\n", cwd.display()));
    ctx.push_str("</status>\n");
    ctx
}

/// Build the static `<loadable-catalog>` system prompt section. Lists every
/// loadable entry by name and description so the system prompt stays fully
/// cacheable. The volatile `enabled=` flags are split out and rendered
/// separately into the session-state tail message — see
/// `build_loadable_enabled_state`. Replaces the separate `<mcp-index>` and
/// `<skill-index>`. Always emits the block so the system prompt instruction
/// to "check `<loadable-catalog>`" is never a lie.
/// Each entry renders as: `  <kind> <name> — <description>`.
///
/// `pub` because `tools/load.rs` calls `crate::turn::build_loadable_catalog`.
pub fn build_loadable_catalog(entries: &[LoadableEntry]) -> String {
    if entries.is_empty() {
        return "\n<loadable-catalog>\n  (none)\n</loadable-catalog>".to_string();
    }
    let lines: Vec<String> = entries
        .iter()
        .map(|e| format!("  {} {} — {}", e.kind.as_str(), e.name, e.description))
        .collect();
    format!(
        "\n<loadable-catalog>\nCall load(name, enabled=true/false) to enable or disable.\n{}\n</loadable-catalog>",
        lines.join("\n")
    )
}

/// Render the volatile enabled-state for loadable entries. Output is meant
/// to live in the session-state tail message, not the system prompt.
/// Wraps the comma-joined names of currently-enabled entries in
/// `<loadable-state>` ... `</loadable-state>`. If nothing is enabled, the
/// inner text is `(none)`.
pub fn build_loadable_enabled_state(entries: &[LoadableEntry]) -> String {
    let names: Vec<&str> = entries
        .iter()
        .filter(|e| e.enabled)
        .map(|e| e.name.as_str())
        .collect();
    let inner = if names.is_empty() {
        "(none)".to_string()
    } else {
        names.join(", ")
    };
    format!("<loadable-state>\nenabled: {}\n</loadable-state>", inner)
}

/// Build the `<permission-status>` block summarising the active allow / deny
/// lists. Empty string when both lists are empty.
///
/// `pub` for parity with `build_loadable_catalog`; consumed by
/// `build_session_state` (the session-state tail message).
pub fn build_permission_status(allow: &[String], deny: &[String]) -> String {
    if allow.is_empty() && deny.is_empty() {
        return String::new();
    }
    let mut lines = vec![];
    if !allow.is_empty() {
        lines.push(format!("Auto-approved: {}", allow.join(", ")));
    }
    if !deny.is_empty() {
        lines.push(format!("Blocked: {}", deny.join(", ")));
    }
    lines.push("Anything else requires user approval.".to_string());
    format!(
        "\n<permission-status>\n{}\n</permission-status>",
        lines.join("\n")
    )
}

/// Assemble the session-state message body that lives at the tail of every
/// step (uncached, free to mutate per-turn / per-step). Fragments:
/// `<status>`-style status block, the optional `<permission-status>`, and
/// the loadable enabled-state. When `permission_status` is empty it is
/// omitted entirely (no blank line).
pub fn build_session_state(status: &str, permission_status: &str, enabled_state: &str) -> String {
    let mut out = String::from("<session-state>\n");
    out.push_str(status);
    if !permission_status.is_empty() {
        out.push_str(permission_status);
    }
    out.push_str(enabled_state);
    out.push_str("</session-state>\n");
    out
}

/// Wrap a discovered `McpToolDef` (from the registry) plus its live
/// `McpClient` in a `Box<dyn Tool>` the streaming loop can push into its
/// `tools` vec. `pub(super)` — only the loop in `turn/mod.rs` calls it.
pub(super) fn make_mcp_tool_def(def: &McpToolDef, client: Arc<McpClient>) -> Box<dyn Tool> {
    let name = def.name.clone();
    let description = def.description.clone();
    let schema = def.schema.clone();
    let server = def.server_name.clone();
    let tool_name = def.name.clone();
    Box::new(crate::types::ToolDef {
        name: name.clone(),
        description,
        parameters: schema,
        execute: Box::new(move |args| {
            let client = client.clone();
            let name = name.clone();
            Box::pin(async move {
                client
                    .call_tool(&name, args)
                    .await
                    .map_err(crate::tools::ToolError::Internal)
            })
        }),
        permission: Some(Box::new(move |_args| {
            crate::permission::mcp_permission(&server, &tool_name)
        })),
    })
}

/// Assemble the per-turn dynamic system prompt: base instructions +
/// `<loadable-catalog>`. The volatile fragments (`<permission-status>` and
/// the loadable `enabled=` flags) have been moved into the session-state
/// tail message so this prompt is fully cacheable within a session. The
/// pre_turn hook call site lives in `turn/mod.rs` (after this prompt is
/// built) — this function is pure: it does not invoke hooks.
///
/// `pub(super)` — only the loop in `turn/mod.rs` builds the dynamic prompt.
pub(super) fn build_dynamic_system_prompt(
    base: &str,
    entries: &[LoadableEntry],
    _hooks: &[Box<dyn Hook>],
) -> String {
    let catalog = build_loadable_catalog(entries);
    format!("{}{}", base, catalog)
}
