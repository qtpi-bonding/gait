//! `/skill` registry scans and load/unload dispatch, shared by both REPL
//! flavors: the rich REPL's `repl::driver::ReplSource` and the plain REPL
//! (`plain.rs`). Extracted so the two front ends drive one implementation
//! instead of two copies drifting apart (Task 4, gait-min-composition plan).
//! No rich-TUI dependency here — pure `gait_core`/`gait_ops` + `std::fs`, so
//! this module stays compiled in every REPL build.

/// `scan_skills` is `gait_core`'s public API; mcp/plugin have no public scan
/// helper (their scanners are `pub(crate)` in gait-core), so these read the
/// canonical disk layout directly — the same shape
/// `crates/gait-cli/src/cli/admin.rs`'s own disk-first branches use.
pub(crate) fn scan_skills(gait_dir: &std::path::Path) -> Vec<String> {
    gait_core::skills::scan_skills(gait_dir)
        .into_iter()
        .map(|s| s.name)
        .collect()
}

pub(crate) fn scan_mcp(gait_dir: &std::path::Path) -> Vec<String> {
    let mut names = vec![];
    if let Ok(rd) = std::fs::read_dir(gait_dir.join("mcp")) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names
}

pub(crate) fn scan_plugins(gait_dir: &std::path::Path) -> Vec<String> {
    let mut names = vec![];
    if let Ok(rd) = std::fs::read_dir(gait_dir.join("plugins")) {
        for entry in rd.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// Currently-loaded names for `/skill`'s listing: folds `Toggle` events
/// straight off the session's log. A simplification vs. the real
/// `ActiveAgent`/`AgentState` projection (`pub(crate)` in gait-core, not
/// reachable from this crate) — good enough for a REPL listing, not used for
/// any enforcement decision.
pub(crate) fn currently_loaded(
    gait_dir: &std::path::Path,
    session_id: &str,
) -> std::collections::HashSet<String> {
    use gait_core::proto::gait::log::v1::{event, payload, ToggleKind};
    let tx_path = gait_core::session::live::log_path(gait_dir, session_id);
    let events = gait_core::session::log::read_events(&tx_path).unwrap_or_default();
    let mut loaded = std::collections::HashSet::new();
    for e in &events {
        if let Some(event::Body::Payload(p)) = &e.body {
            if let Some(payload::Kind::Toggle(t)) = &p.kind {
                if matches!(
                    ToggleKind::try_from(t.kind).unwrap_or(ToggleKind::Unspecified),
                    ToggleKind::Skill | ToggleKind::Mcp | ToggleKind::Plugin
                ) {
                    if t.enabled {
                        loaded.insert(t.name.clone());
                    } else {
                        loaded.remove(&t.name);
                    }
                }
            }
        }
    }
    loaded
}

/// Render the `/skill` (no-args) listing: `\n`-joined lines, one per
/// registered skill/mcp/plugin, with its loaded/available status. Callers
/// adapt line endings for their own output surface (the rich REPL's raw
/// writer wants `\r\n`; the plain REPL wants plain `\n`).
pub(crate) fn list_text(gait_dir: &std::path::Path, session_id: &str) -> String {
    let loaded = currently_loaded(gait_dir, session_id);
    let mut out = String::new();
    for (kind, names) in [
        ("skill", scan_skills(gait_dir)),
        ("mcp", scan_mcp(gait_dir)),
        ("plugin", scan_plugins(gait_dir)),
    ] {
        for name in names {
            let status = if loaded.contains(&name) {
                "loaded"
            } else {
                "available"
            };
            out.push_str(&format!("  {} {} ({})\n", kind, name, status));
        }
    }
    if out.is_empty() {
        out.push_str("  (none registered)\n");
    }
    out
}

/// Drive `gait_ops::extension::{load,unload}` in-process for `/skill
/// load|unload <name>` (mirrors `/unlock`'s op-call pattern, since those
/// mutate a live session's log). Returns `Err(message)` for both a bad verb
/// and an op failure, so the caller can print one uniform `[gait] ...` line.
pub(crate) fn load_or_unload(
    gait_dir: &std::path::Path,
    session_id: &str,
    verb: &str,
    name: &str,
) -> Result<(), String> {
    if name.is_empty() || (verb != "load" && verb != "unload") {
        return Err("usage: /skill [load|unload <name>]".to_string());
    }
    let kind = if scan_skills(gait_dir).contains(&name.to_string()) {
        Some("skill")
    } else if scan_mcp(gait_dir).contains(&name.to_string()) {
        Some("mcp")
    } else if scan_plugins(gait_dir).contains(&name.to_string()) {
        Some("plugin")
    } else {
        None
    };
    let Some(kind) = kind else {
        return Err(format!("/skill {}: '{}' not found in any registry", verb, name));
    };
    let ext_kind = match kind {
        "skill" => gait_ops::extension::ExtKind::Skill,
        "mcp" => gait_ops::extension::ExtKind::Mcp,
        "plugin" => gait_ops::extension::ExtKind::Plugin,
        _ => unreachable!(),
    };
    let params = gait_ops::extension::LoadParams {
        gait_dir: gait_dir.to_path_buf(),
        session_id: session_id.to_string(),
        kind: ext_kind,
        name: name.to_string(),
    };
    let result = if verb == "load" {
        gait_ops::extension::load(params)
    } else {
        gait_ops::extension::unload(params)
    };
    result
        .map(|_| ())
        .map_err(|e| format!("/skill {} {} failed: {e}", verb, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_text_reports_none_registered_when_empty() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(list_text(d.path(), "s1"), "  (none registered)\n");
    }

    #[test]
    fn load_or_unload_rejects_unknown_name() {
        let d = tempfile::tempdir().unwrap();
        let err = load_or_unload(d.path(), "s1", "load", "nope").unwrap_err();
        assert!(err.contains("not found in any registry"), "{err}");
    }

    #[test]
    fn load_or_unload_rejects_bad_verb() {
        let d = tempfile::tempdir().unwrap();
        let err = load_or_unload(d.path(), "s1", "frobnicate", "x").unwrap_err();
        assert!(err.contains("usage:"), "{err}");
    }
}
