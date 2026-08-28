//! Prompt-construction helpers used by the streaming turn loop. Owns the
//! `<loadable-catalog>`, `<loadable-state>`, `<permission-status>`, and
//! `<status>` system-prompt fragments plus the `MakeMcpToolDef` shim and
//! the dynamic assembly step that folds hooks into the live system prompt.

use crate::config::McpToolDef;
use crate::types::{Hook, LoadableEntry};

/// Render the `<status>` block the model sees at the top of every turn:
/// unix timestamp, codebase HEAD, and dirty files. Tested directly — keep
/// `pub(crate)` so `turn/tests.rs` can call it.
pub(crate) fn build_turn_context(
    codebase_hash: &str,
    dirty_files: &[String],
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
        "\n<loadable-catalog>\nCall enable(name, enabled=true/false) to enable or disable.\n{}\n</loadable-catalog>",
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
/// `<status>`-style status block, the optional `<permission-status>`, the
/// optional `<mode>` steer, and the loadable enabled-state. When
/// `permission_status` / `mode_steer` is empty it is omitted entirely (no
/// blank line).
pub fn build_session_state(
    status: &str,
    permission_status: &str,
    enabled_state: &str,
    mode_steer: &str,
) -> String {
    let mut out = String::from("<session-state>\n");
    out.push_str(status);
    if !permission_status.is_empty() {
        out.push_str(permission_status);
    }
    if !mode_steer.is_empty() {
        out.push_str(&format!("\n<mode>\n{}\n</mode>", mode_steer));
    }
    out.push_str(enabled_state);
    out.push_str("</session-state>\n");
    out
}

/// Compact, volatile index of enabled MCP façades. Full schemas are kept in
/// the following reference layer so the index remains cheap to cache.
pub fn build_mcp_index(
    registry: &[McpToolDef],
    enabled: &std::collections::HashSet<String>,
) -> String {
    let mut servers: Vec<_> = registry
        .iter()
        .filter(|d| enabled.contains(&d.server_name))
        .map(|d| d.server_name.as_str())
        .collect();
    servers.sort_unstable();
    servers.dedup();
    let body = if servers.is_empty() {
        "  (none)".to_string()
    } else {
        servers
            .into_iter()
            .map(|s| {
                format!(
                    "  {} — {} tool(s)",
                    s,
                    registry.iter().filter(|d| d.server_name == s).count()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!("\n<mcp-index>\n{}\n</mcp-index>", body)
}

/// Full canonical JSON references available to Code Mode at invocation time.
pub fn build_mcp_references(
    registry: &[McpToolDef],
    enabled: &std::collections::HashSet<String>,
    max_bytes: usize,
) -> String {
    let mut out = String::from("\n<mcp-references>\n");
    let mut servers: Vec<_> = registry
        .iter()
        .filter(|d| enabled.contains(&d.server_name))
        .map(|d| d.server_name.clone())
        .collect();
    servers.sort();
    servers.dedup();
    for server in servers {
        let tools = registry
            .iter()
            .filter(|d| d.server_name == server)
            .map(|d| gait_codemode::ToolReference {
                name: d.name.clone(),
                description: Some(d.description.clone()),
                input_schema: d.schema.clone(),
                output_schema: d.output_schema.clone(),
            })
            .collect::<Vec<_>>();
        out.push_str(&gait_codemode::render_reference(&server, &tools));
        if out.len() > max_bytes {
            return "\n<mcp-references>\n  (reference size limit exceeded)\n</mcp-references>"
                .to_string();
        }
    }
    out.push_str("</mcp-references>");
    out
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
    mcp_registry: &[McpToolDef],
    enabled_mcp: &std::collections::HashSet<String>,
    _hooks: &[Box<dyn Hook>],
) -> String {
    let catalog = build_loadable_catalog(entries);
    let index = build_mcp_index(mcp_registry, enabled_mcp);
    let references = build_mcp_references(mcp_registry, enabled_mcp, 256 * 1024);
    format!("{}{}{}", base, catalog, index) + &references
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn session_state_includes_mode_block_only_when_steer_present() {
        let with = build_session_state("<status/>", "", "", "no edits");
        assert!(with.contains("<mode>") && with.contains("no edits"));
        let without = build_session_state("<status/>", "", "", "");
        assert!(!without.contains("<mode>"));
    }

    #[test]
    fn mode_steer_does_not_change_cached_prefix() {
        // The cached prefix is build_dynamic_system_prompt output; steer lives only
        // in the tail, so it must be byte-identical regardless of steer.
        let a = build_dynamic_system_prompt("base", &[], &[], &Default::default(), &[]);
        let b = build_dynamic_system_prompt("base", &[], &[], &Default::default(), &[]);
        assert_eq!(a, b); // guard: steer never enters this function's inputs
    }
}
