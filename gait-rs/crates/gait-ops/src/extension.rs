//! Extension (skill/mcp/plugin) lifecycle verbs: `load`, `unload`, `register`,
//! `unregister`, `enable`, `disable`.
//!
//! Sources: `crates/gait-cli/src/cli/session.rs` (`cmd_load`, `cmd_unload`,
//! `resolve_load_targets`) and `crates/gait-cli/src/cli/admin.rs`
//! (`cmd_register`, `cmd_unregister`, `cmd_enable_disable_inner`, and the
//! frontmatter / config helpers they use). All side effects — disk writes
//! (MCP `mcp/<name>.json`, skills `skills/<name>/SKILL.md`, plugin
//! `plugins/<name>/`), agent frontmatter lists, `gait.jsonc` array entries,
//! and `Toggle` / `Registry` events on session logs — move here. Frontmatter
//! helpers (`set_frontmatter_bool`, `append_frontmatter_list`,
//! `remove_frontmatter_list`, `read_config_as_value`) are private to this
//! module, mirroring how `workspace.rs` keeps its `enclosing_git_repo` /
//! `gitignore_line_for` / `is_gait_workspace` / `init_decision` private to
//! itself.
use crate::OpError;
use std::path::{Path, PathBuf};

// -----------------------------------------------------------------------------
// load / unload  — append a Toggle event to a session's log outside a turn
// -----------------------------------------------------------------------------

/// Kind of extension the load/unload op targets. The same convention the
/// `Toggle`/`Registry` event payloads use (`"skill"` | `"mcp"` | `"plugin"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtKind {
    Skill,
    Mcp,
    Plugin,
}

impl ExtKind {
    fn as_str(self) -> &'static str {
        match self {
            ExtKind::Skill => "skill",
            ExtKind::Mcp => "mcp",
            ExtKind::Plugin => "plugin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadParams {
    pub gait_dir: PathBuf,
    pub session_id: String,
    pub kind: ExtKind,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct LoadOutcome {
    pub tx_path: PathBuf,
    pub enabled: bool,
}

/// Outcome of `load` or `unload`. Carries the log path the op wrote (so the
/// adapter can render the relative path under `--json`) and the enabled bit
/// it stamped.
#[derive(Debug)]
pub struct ToggleOutcome {
    pub tx_path: PathBuf,
    pub enabled: bool,
}

/// Resolve `tx_path` / `session_dir` for `session_id` and validate that the
/// target `name` is present in the global registry for `kind`. The full
/// agent_whitelist enforcement (Phase 3's `resolve_visible_set`) is not
/// reachable from this op context — the agent_whitelist is normally threaded
/// through Profile/ResolvedProfile at turn-setup time, not persisted anywhere
/// reloadable from just `gait_dir` + `session_id`. That's a known,
/// narrower-than-ideal scope here; the in-turn `enable` enforcement at
/// `turn/execute.rs:370` and the `--scope session` register/unregister writes
/// are the required Phase 3 enforcement points per the plan. This is the
/// minimum: a name not present in any registry must error clearly.
fn resolve_load_targets(
    kind: ExtKind,
    name: &str,
    gait_dir: &Path,
    session_id: &str,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    // `gait_state::mcp` / `gait_state::wasm_plugins` are `pub(crate)` (not
    // reachable from this crate), so mcp/plugin existence is checked the same
    // way admin.rs's own disk-first branches did: by the canonical disk path,
    // not a scan helper. `skills::scan_skills` is public and used directly.
    let in_global_registry: bool = match kind {
        ExtKind::Skill => gait_state::skills::scan_skills(gait_dir)
            .iter()
            .any(|s| s.name == name),
        ExtKind::Mcp => gait_dir.join("mcp").join(format!("{}.json", name)).exists(),
        ExtKind::Plugin => gait_dir.join("plugins").join(name).exists(),
    };
    if !in_global_registry {
        return Err(coded_error::into_anyhow(OpError::NotFound(format!(
            "{} '{}' is not registered; run `gait register {} {}` first",
            kind.as_str(),
            name,
            kind.as_str(),
            name
        ))));
    }

    let tx_path = gait_state::session::live::log_path(gait_dir, session_id);
    let session_dir = gait_state::session::live::worktree_dir(gait_dir, session_id);
    Ok((tx_path, session_dir))
}

/// Append a `Toggle(enabled=true)` event to `session_id`'s log outside a
/// running turn. `gait_state::session::log::write_toggle` itself refuses to
/// append when the session is live (concurrency contract — see
/// `src/session/log/mod.rs`); that refusal surfaces as `OpError::Conflict`
/// here.
pub fn load(params: LoadParams) -> anyhow::Result<ToggleOutcome> {
    let (tx_path, session_dir) = resolve_load_targets(
        params.kind,
        &params.name,
        &params.gait_dir,
        &params.session_id,
    )?;
    gait_state::session::log::write_toggle(
        &tx_path,
        &session_dir,
        params.kind.as_str(),
        &params.name,
        true,
    )
    .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
    Ok(ToggleOutcome {
        tx_path,
        enabled: true,
    })
}

/// Append a `Toggle(enabled=false)` event to `session_id`'s log outside a
/// running turn. Same live-session guard as [`load`].
pub fn unload(params: LoadParams) -> anyhow::Result<ToggleOutcome> {
    let (tx_path, session_dir) = resolve_load_targets(
        params.kind,
        &params.name,
        &params.gait_dir,
        &params.session_id,
    )?;
    gait_state::session::log::write_toggle(
        &tx_path,
        &session_dir,
        params.kind.as_str(),
        &params.name,
        false,
    )
    .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
    Ok(ToggleOutcome {
        tx_path,
        enabled: false,
    })
}

// -----------------------------------------------------------------------------
// register / unregister  — three scopes: global (disk) | agent (frontmatter
// list) | session (log Registry event)
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct RegisterParams {
    pub gait_dir: PathBuf,
    pub kind: String,
    pub name: String,
    /// MCP URL (mcp kind).
    pub url: Option<String>,
    /// MCP command program (mcp kind).
    pub command: Option<String>,
    /// MCP args string (mcp kind).
    pub args: Option<String>,
    /// MCP compat spawn string (mcp kind).
    pub spawn: Option<String>,
    /// MCP compat URL flag (mcp kind).
    pub connect: Option<String>,
    /// Skill description (skill kind).
    pub description: Option<String>,
    /// Skill content (skill kind).
    pub content: Option<String>,
    /// Skill path (skill kind).
    pub path: Option<String>,
    /// Plugin wasm source path (plugin kind).
    pub wasm: Option<String>,
    /// Scope: `"global"` (default), `"agent"`, or `"session"`.
    pub scope: Option<String>,
    /// Path to agent .md file (only meaningful with `--scope agent`).
    pub agent: Option<PathBuf>,
    /// Session id (only meaningful with `--scope session`).
    pub session: Option<String>,
}

/// Outcome of `register`. Reports which storage the op actually wrote to so
/// the adapter can render the matching user-facing message
/// ("Registered mcp", "Added to agent whitelist", "Added to session
/// whitelist").
#[derive(Debug)]
pub struct RegisterOutcome {
    pub scope: &'static str,
    /// Path of the on-disk entry (for `global` scope), the agent file (for
    /// `agent` scope), or the session log file (for `session` scope).
    pub written_to: PathBuf,
    /// Free-form hint for the adapter (e.g. "→ Run `gait enable mcp X` to
    /// activate it."). Empty when not applicable.
    pub hint: String,
}

#[derive(Debug, Clone, Default)]
pub struct UnregisterParams {
    pub gait_dir: PathBuf,
    pub kind: String,
    pub name: String,
    pub scope: Option<String>,
    pub agent: Option<PathBuf>,
    pub session: Option<String>,
}

#[derive(Debug)]
pub struct UnregisterOutcome {
    pub scope: &'static str,
    pub written_to: PathBuf,
}

/// Register an extension. Mirrors the original `cmd_register` body: agent
/// scope only touches the agent file's frontmatter list; session scope only
/// appends a `Registry(listed=true)` event to one session's log; the
/// remaining branches write the canonical disk entry (MCPs → `mcp/<name>.json`,
/// skills → `skills/<name>/SKILL.md`, plugins → `plugins/<name>/`). The op
/// returns `anyhow::Result<RegisterOutcome>`; the adapter renders the
/// per-scope success message.
pub fn register(params: RegisterParams) -> anyhow::Result<RegisterOutcome> {
    let kind = params.kind.as_str();
    let name = params.name.as_str();

    // Phase 1: --scope agent only touches the agent file's frontmatter list.
    // It never falls through to the global content-store branch below.
    if params.scope.as_deref() == Some("agent") {
        let agent_path = params.agent.clone().ok_or_else(|| {
            coded_error::into_anyhow(OpError::Conflict("--scope agent requires --agent <path>".to_string()))
        })?;
        let list_key = match kind {
            "skill" => "skills",
            "mcp" => "mcp",
            "plugin" => "plugins",
            other => {
                return Err(coded_error::into_anyhow(OpError::Conflict(format!(
                    "unknown kind '{}': use mcp, skill, or plugin",
                    other
                ))))
            }
        };
        let content = std::fs::read_to_string(&agent_path).map_err(|e| {
            coded_error::into_anyhow(OpError::Conflict(format!(
                "failed to read agent file '{}': {}",
                agent_path.display(),
                e
            )))
        })?;
        let new_content = append_frontmatter_list(&content, list_key, name)
            .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
        std::fs::write(&agent_path, new_content).map_err(|e| {
            coded_error::into_anyhow(OpError::Conflict(format!(
                "failed to write agent file '{}': {}",
                agent_path.display(),
                e
            )))
        })?;
        return Ok(RegisterOutcome {
            scope: "agent",
            written_to: agent_path,
            hint: String::new(),
        });
    }

    // Phase 2: --scope session appends a Registry event to one session's own
    // log. Never falls through to the global content-store branch below.
    if params.scope.as_deref() == Some("session") {
        let session_id = params.session.clone().ok_or_else(|| {
            coded_error::into_anyhow(OpError::Conflict("--scope session requires --session <id>".to_string()))
        })?;
        let tx_path = gait_state::session::live::log_path(&params.gait_dir, &session_id);
        let session_dir = gait_state::session::live::worktree_dir(&params.gait_dir, &session_id);
        gait_state::session::log::write_registry(&tx_path, &session_dir, kind, name, true)
            .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
        return Ok(RegisterOutcome {
            scope: "session",
            written_to: tx_path,
            hint: String::new(),
        });
    }

    // Global scope: write the canonical disk entry.
    match kind {
        "mcp" => {
            let mcp_dir = params.gait_dir.join("mcp");
            std::fs::create_dir_all(&mcp_dir)
                .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("could not create mcp/ dir: {e}"))))?;
            let disk_path = mcp_dir.join(format!("{}.json", name));
            if disk_path.exists() {
                return Err(coded_error::into_anyhow(OpError::Conflict(format!(
                    "mcp '{}' already registered at {}. Use 'gait unregister mcp {}' first.",
                    name,
                    disk_path.display(),
                    name
                ))));
            }
            let entry = if let Some(cmd) = &params.command {
                let cmd_args: Vec<serde_json::Value> = params
                    .args
                    .as_deref()
                    .map(|s| {
                        s.split_whitespace()
                            .map(|a| serde_json::Value::String(a.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                serde_json::json!({ "command": cmd, "args": cmd_args, "enabled": false })
            } else if let Some(spawn) = &params.spawn {
                let mut parts = spawn.split_whitespace();
                let prog = parts.next().unwrap_or("").to_string();
                let rest: Vec<serde_json::Value> = parts
                    .map(|s| serde_json::Value::String(s.to_string()))
                    .collect();
                serde_json::json!({ "command": prog, "args": rest, "enabled": false })
            } else if let Some(url) = params.url.as_ref().or(params.connect.as_ref()) {
                serde_json::json!({ "url": url, "enabled": false })
            } else {
                return Err(coded_error::into_anyhow(OpError::Conflict(
                    "gait register mcp: requires --command <prog> [--args \"<args>\"], --spawn <cmd>, or --url <url>"
                        .to_string(),
                )));
            };
            let final_entry = if let Some(desc) = &params.description {
                let mut obj = entry.as_object().unwrap().clone();
                obj.insert(
                    "description".into(),
                    serde_json::Value::String(desc.clone()),
                );
                serde_json::Value::Object(obj)
            } else {
                entry
            };
            std::fs::write(
                &disk_path,
                serde_json::to_string_pretty(&final_entry)
                    .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?,
            )
            .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
            Ok(RegisterOutcome {
                scope: "global",
                written_to: disk_path,
                hint: format!("Run `gait enable mcp {}` to activate it.", name),
            })
        }
        "skill" => {
            let skills_root = params.gait_dir.join("skills");
            std::fs::create_dir_all(&skills_root)
                .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("could not create skills/ dir: {e}"))))?;
            let skill_dir = skills_root.join(name);
            if skill_dir.exists() {
                return Err(coded_error::into_anyhow(OpError::Conflict(format!(
                    "skill '{}' already registered at {}. Use 'gait unregister skill {}' first.",
                    name,
                    skill_dir.display(),
                    name
                ))));
            }
            let description = params.description.as_deref().ok_or_else(|| {
                coded_error::into_anyhow(OpError::Conflict("gait register skill: requires --description <text>".to_string()))
            })?;
            let content = if let Some(path) = &params.path {
                std::fs::read_to_string(path).map_err(|e| {
                    coded_error::into_anyhow(OpError::Conflict(format!("failed to read skill file '{}': {}", path, e)))
                })?
            } else if let Some(content) = &params.content {
                content.clone()
            } else {
                return Err(coded_error::into_anyhow(OpError::Conflict(
                    "gait register skill: requires --path <file> or --content <text>".to_string(),
                )));
            };
            std::fs::create_dir_all(&skill_dir).map_err(|e| {
                coded_error::into_anyhow(OpError::Conflict(format!("could not create skills/{} dir: {e}", name)))
            })?;
            let skill_md = skill_dir.join("SKILL.md");
            std::fs::write(
                &skill_md,
                format!(
                    "---\nname: {}\ndescription: {}\nenabled: false\n---\n\n{}",
                    name, description, content
                ),
            )
            .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
            Ok(RegisterOutcome {
                scope: "global",
                written_to: skill_md,
                hint: format!("Run `gait enable skill {}` to activate it.", name),
            })
        }
        "plugin" => {
            let wasm_src = params.wasm.as_deref().ok_or_else(|| {
                coded_error::into_anyhow(OpError::Conflict("gait register plugin: requires --wasm <path>".to_string()))
            })?;
            let wasm_src_path = Path::new(wasm_src);
            if !wasm_src_path.exists() {
                return Err(coded_error::into_anyhow(OpError::Conflict(format!(
                    "wasm file not found: {}",
                    wasm_src
                ))));
            }
            let plugin_dir = params.gait_dir.join("plugins").join(name);
            if plugin_dir.exists() {
                return Err(coded_error::into_anyhow(OpError::Conflict(format!(
                    "plugin '{}' already registered at {}. Use 'gait unregister plugin {}' first.",
                    name,
                    plugin_dir.display(),
                    name
                ))));
            }
            std::fs::create_dir_all(&plugin_dir).map_err(|e| {
                coded_error::into_anyhow(OpError::Conflict(format!("could not create plugins/{} dir: {e}", name)))
            })?;
            let wasm_filename = wasm_src_path
                .file_name()
                .ok_or_else(|| coded_error::into_anyhow(OpError::Conflict(format!("invalid wasm path: {}", wasm_src))))?;
            let wasm_dest = plugin_dir.join(wasm_filename);
            std::fs::copy(wasm_src_path, &wasm_dest)
                .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("could not copy wasm file: {e}"))))?;
            let wasm_rel = wasm_filename.to_string_lossy();
            let description = params.description.as_deref().unwrap_or("No description.");
            let plugin_md = plugin_dir.join("PLUGIN.md");
            std::fs::write(
                &plugin_md,
                format!(
                    "---\nname: {}\nwasm: {}\nenabled: false\nallow_exec: []\nallow_fs: []\n---\n\n# {}\n\n{}\n",
                    name, wasm_rel, name, description
                ),
            )
            .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
            Ok(RegisterOutcome {
                scope: "global",
                written_to: plugin_md,
                hint: format!("Run `gait enable plugin {}` to activate it.", name),
            })
        }
        other => Err(coded_error::into_anyhow(OpError::Conflict(format!(
            "unknown kind '{}': use mcp, skill, or plugin",
            other
        )))),
    }
}

/// Unregister an extension. Same per-scope branching as [`register`]: agent
/// scope removes from the agent file's frontmatter list; session scope
/// appends a `Registry(listed=false)` event; the remaining branches remove
/// the disk entry (MCP file, skill dir, plugin dir) or fall through to
/// `gait.jsonc` for legacy entries.
pub fn unregister(params: UnregisterParams) -> anyhow::Result<UnregisterOutcome> {
    let kind = params.kind.as_str();
    let name = params.name.as_str();

    // Phase 1: --scope agent only touches the agent file's frontmatter list.
    if params.scope.as_deref() == Some("agent") {
        let agent_path = params.agent.clone().ok_or_else(|| {
            coded_error::into_anyhow(OpError::Conflict("--scope agent requires --agent <path>".to_string()))
        })?;
        let list_key = match kind {
            "skill" => "skills",
            "mcp" => "mcp",
            "plugin" => "plugins",
            other => {
                return Err(coded_error::into_anyhow(OpError::Conflict(format!(
                    "unknown kind '{}': use mcp, skill, or plugin",
                    other
                ))))
            }
        };
        let content = std::fs::read_to_string(&agent_path).map_err(|e| {
            coded_error::into_anyhow(OpError::Conflict(format!(
                "failed to read agent file '{}': {}",
                agent_path.display(),
                e
            )))
        })?;
        let new_content = remove_frontmatter_list(&content, list_key, name)
            .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
        std::fs::write(&agent_path, new_content).map_err(|e| {
            coded_error::into_anyhow(OpError::Conflict(format!(
                "failed to write agent file '{}': {}",
                agent_path.display(),
                e
            )))
        })?;
        return Ok(UnregisterOutcome {
            scope: "agent",
            written_to: agent_path,
        });
    }

    // Phase 2: --scope session appends a Registry(listed=false) event to one
    // session's own log.
    if params.scope.as_deref() == Some("session") {
        let session_id = params.session.clone().ok_or_else(|| {
            coded_error::into_anyhow(OpError::Conflict("--scope session requires --session <id>".to_string()))
        })?;
        let tx_path = gait_state::session::live::log_path(&params.gait_dir, &session_id);
        let session_dir = gait_state::session::live::worktree_dir(&params.gait_dir, &session_id);
        gait_state::session::log::write_registry(&tx_path, &session_dir, kind, name, false)
            .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
        return Ok(UnregisterOutcome {
            scope: "session",
            written_to: tx_path,
        });
    }

    // Disk-first for mcp, plugin, and skill.
    if kind == "mcp" {
        let disk_path = params.gait_dir.join("mcp").join(format!("{}.json", name));
        if disk_path.exists() {
            std::fs::remove_file(&disk_path).map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
            return Ok(UnregisterOutcome {
                scope: "global",
                written_to: disk_path,
            });
        }
    }
    if kind == "plugin" {
        let plugin_dir = params.gait_dir.join("plugins").join(name);
        if plugin_dir.exists() {
            std::fs::remove_dir_all(&plugin_dir).map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
            return Ok(UnregisterOutcome {
                scope: "global",
                written_to: plugin_dir,
            });
        }
        return Err(coded_error::into_anyhow(OpError::NotFound(format!("plugin '{}' not found", name))));
    }
    if kind == "skill" {
        let skill_dir = params.gait_dir.join("skills").join(name);
        if skill_dir.exists() {
            std::fs::remove_dir_all(&skill_dir).map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
            return Ok(UnregisterOutcome {
                scope: "global",
                written_to: skill_dir,
            });
        }
        return Err(coded_error::into_anyhow(OpError::NotFound(format!("skill '{}' not found", name))));
    }

    // Fall back to gait.jsonc for legacy entries.
    let array_key = match kind {
        "mcp" => "mcp_servers",
        "skill" => "skills",
        other => {
            return Err(coded_error::into_anyhow(OpError::Conflict(format!(
                "unknown kind '{}': use mcp, skill, or plugin",
                other
            ))))
        }
    };
    let config_path = params.gait_dir.join("gait.jsonc");
    let mut val =
        read_config_as_value(&config_path).map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
    if let Some(arr) = val[array_key].as_array_mut() {
        arr.retain(|v| v.get("name").and_then(|n| n.as_str()) != Some(name));
    }
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&val).map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?,
    )
    .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
    Ok(UnregisterOutcome {
        scope: "config",
        written_to: config_path,
    })
}

// -----------------------------------------------------------------------------
// enable / disable
// -----------------------------------------------------------------------------

/// Outcome of `enable` / `disable`. Reports the on-disk file the op rewrote
/// (so the adapter can render the same path under `--json`) and which side
/// effect (disk vs. legacy `gait.jsonc`) won.
#[derive(Debug)]
pub struct EnableOutcome {
    pub written_to: PathBuf,
    /// `"disk"` if the canonical mcp/skill/plugin file was rewritten,
    /// `"config"` if we fell through to `gait.jsonc` (legacy entries).
    pub target: &'static str,
}

#[derive(Debug, Clone)]
pub struct EnableParams {
    pub gait_dir: PathBuf,
    pub kind: String,
    pub name: String,
}

/// Flip an extension's `enabled` flag. Mirrors `cmd_enable_disable_inner`:
/// disk-file path for mcp / plugin / skill; falls through to the
/// `gait.jsonc` legacy `mcp_servers` / `skills` arrays when the disk file
/// doesn't exist. `pub(crate)` — the public entry points for toggling
/// extensions are `enable`/`disable`; this is just the shared backing impl.
pub(crate) fn set_enabled(params: EnableParams, enabled: bool) -> anyhow::Result<EnableOutcome> {
    // Disk-file path for mcp, plugin, and skill kinds.
    let disk_path: Option<PathBuf> = match params.kind.as_str() {
        "mcp" => {
            let p = params
                .gait_dir
                .join("mcp")
                .join(format!("{}.json", params.name));
            if p.exists() {
                Some(p)
            } else {
                None
            }
        }
        "plugin" => {
            let p = params
                .gait_dir
                .join("plugins")
                .join(&params.name)
                .join("PLUGIN.md");
            if p.exists() {
                Some(p)
            } else {
                None
            }
        }
        "skill" => {
            let p = params
                .gait_dir
                .join("skills")
                .join(&params.name)
                .join("SKILL.md");
            if p.exists() {
                Some(p)
            } else {
                None
            }
        }
        _ => None,
    };

    if let Some(path) = disk_path {
        match params.kind.as_str() {
            "mcp" => {
                let mut val: serde_json::Value = serde_json::from_str(
                    &std::fs::read_to_string(&path)
                        .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?,
                )
                .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
                val["enabled"] = serde_json::Value::Bool(enabled);
                std::fs::write(
                    &path,
                    serde_json::to_string_pretty(&val)
                        .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?,
                )
                .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
            }
            "plugin" => {
                let content = std::fs::read_to_string(&path)
                    .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
                let new_content = set_frontmatter_bool(&content, "enabled", enabled)
                    .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
                std::fs::write(&path, new_content)
                    .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
            }
            "skill" => {
                let content = std::fs::read_to_string(&path)
                    .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
                let new_content = set_frontmatter_bool(&content, "enabled", enabled)
                    .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
                std::fs::write(&path, new_content)
                    .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
            }
            _ => unreachable!(),
        }
        return Ok(EnableOutcome {
            written_to: path,
            target: "disk",
        });
    }

    // Fall back to gait.jsonc for legacy mcp/skill entries.
    let array_key = match params.kind.as_str() {
        "mcp" => "mcp_servers",
        "skill" => "skills",
        other => {
            return Err(coded_error::into_anyhow(OpError::Conflict(format!(
                "unknown kind '{}': use mcp, skill, or plugin",
                other
            ))))
        }
    };
    let config_path = params.gait_dir.join("gait.jsonc");
    let mut val =
        read_config_as_value(&config_path).map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
    let arr = val[array_key]
        .as_array_mut()
        .ok_or_else(|| coded_error::into_anyhow(OpError::Conflict(format!("{} is not an array", array_key))))?;
    let entry = arr
        .iter_mut()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some(params.name.as_str()))
        .ok_or_else(|| {
            coded_error::into_anyhow(OpError::NotFound(format!(
                "{} '{}' not found; run `gait register {} {}` first",
                params.kind, params.name, params.kind, params.name
            )))
        })?;
    entry["enabled"] = serde_json::Value::Bool(enabled);
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&val).map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?,
    )
    .map_err(|e| coded_error::into_anyhow(OpError::Conflict(format!("{e}"))))?;
    Ok(EnableOutcome {
        written_to: config_path,
        target: "config",
    })
}

/// Thin wrapper around [`set_enabled`] passing `enabled=true`.
pub fn enable(params: EnableParams) -> anyhow::Result<EnableOutcome> {
    set_enabled(params, true)
}

/// Thin wrapper around [`set_enabled`] passing `enabled=false`.
pub fn disable(params: EnableParams) -> anyhow::Result<EnableOutcome> {
    set_enabled(params, false)
}

// -----------------------------------------------------------------------------
// Private frontmatter / config helpers — relocated from admin.rs. Each is
// only used by register / unregister / enable / disable above.
// -----------------------------------------------------------------------------

/// Read `gait.jsonc` as a JSON value, stripping `//` and `/* */` comments
/// first. Missing file → `{}`. The strip is necessary because we must
/// preserve all other keys on rewrite; a `serde_json::from_str` on a raw
/// jsonc would error and force the caller to drop the file.
fn read_config_as_value(config_path: &Path) -> anyhow::Result<serde_json::Value> {
    let raw = std::fs::read_to_string(config_path).unwrap_or_else(|_| "{}".into());
    let stripped = gait_state::config::strip_jsonc_comments(&raw);
    Ok(serde_json::from_str(&stripped).unwrap_or_default())
}

/// Update a boolean key in YAML frontmatter of a markdown file, preserving
/// the body. Mirrors the original `set_frontmatter_bool` 1-for-1.
fn set_frontmatter_bool(content: &str, key: &str, value: bool) -> anyhow::Result<String> {
    const FENCE: &str = "---";
    if !content.starts_with(FENCE) {
        return Err(anyhow::anyhow!("no frontmatter found"));
    }
    let rest = &content[FENCE.len()..];
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let end = rest
        .find(&format!("\n{}", FENCE))
        .ok_or_else(|| anyhow::anyhow!("unclosed frontmatter"))?;
    let fm_str = &rest[..end];
    let body_start = end + 1 + FENCE.len();
    let body = rest[body_start..]
        .strip_prefix('\n')
        .unwrap_or(&rest[body_start..]);

    // Parse YAML, set the key, serialize back
    let mut yaml: serde_yaml::Value = serde_yaml::from_str(fm_str)
        .map_err(|e| anyhow::anyhow!("frontmatter parse error: {e}"))?;
    if let serde_yaml::Value::Mapping(ref mut map) = yaml {
        map.insert(
            serde_yaml::Value::String(key.to_string()),
            serde_yaml::Value::Bool(value),
        );
    }
    let new_fm = serde_yaml::to_string(&yaml)
        .map_err(|e| anyhow::anyhow!("frontmatter serialize error: {e}"))?;
    Ok(format!("---\n{}---\n{}", new_fm, body))
}

/// Append a value to a YAML list at `key` in frontmatter, creating the key
/// if absent and creating frontmatter entirely if the file has none.
/// Avoids duplicate entries.
fn append_frontmatter_list(content: &str, key: &str, value: &str) -> anyhow::Result<String> {
    const FENCE: &str = "---";
    let (yaml_str, body) = if let Some(rest) = content.strip_prefix(FENCE) {
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        let end = rest
            .find(&format!("\n{}", FENCE))
            .ok_or_else(|| anyhow::anyhow!("unclosed frontmatter"))?;
        let fm_str = &rest[..end];
        let body_start = end + 1 + FENCE.len();
        let body = rest[body_start..]
            .strip_prefix('\n')
            .unwrap_or(&rest[body_start..]);
        (fm_str.to_string(), body.to_string())
    } else {
        (String::new(), content.to_string())
    };

    let mut yaml: serde_yaml::Value = if yaml_str.trim().is_empty() {
        serde_yaml::Value::Mapping(Default::default())
    } else {
        serde_yaml::from_str(&yaml_str)
            .map_err(|e| anyhow::anyhow!("frontmatter parse error: {e}"))?
    };

    if let serde_yaml::Value::Mapping(ref mut map) = yaml {
        let entry = map
            .entry(serde_yaml::Value::String(key.to_string()))
            .or_insert_with(|| serde_yaml::Value::Sequence(Default::default()));
        if let serde_yaml::Value::Sequence(ref mut seq) = entry {
            let already = seq
                .iter()
                .any(|v| v.as_str().map(|s| s == value).unwrap_or(false));
            if !already {
                seq.push(serde_yaml::Value::String(value.to_string()));
            }
        }
    }

    let new_fm = serde_yaml::to_string(&yaml)
        .map_err(|e| anyhow::anyhow!("frontmatter serialize error: {e}"))?;
    if body.is_empty() {
        Ok(format!("---\n{}---\n", new_fm))
    } else {
        Ok(format!("---\n{}---\n{}", new_fm, body))
    }
}

/// Remove a value from the YAML list at `key` in frontmatter. No-op if key
/// or value is absent.
fn remove_frontmatter_list(content: &str, key: &str, value: &str) -> anyhow::Result<String> {
    const FENCE: &str = "---";
    if !content.starts_with(FENCE) {
        return Ok(content.to_string());
    }
    let rest = &content[FENCE.len()..];
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let end = rest
        .find(&format!("\n{}", FENCE))
        .ok_or_else(|| anyhow::anyhow!("unclosed frontmatter"))?;
    let fm_str = &rest[..end];
    let body_start = end + 1 + FENCE.len();
    let body = rest[body_start..]
        .strip_prefix('\n')
        .unwrap_or(&rest[body_start..]);

    let mut yaml: serde_yaml::Value = if fm_str.trim().is_empty() {
        serde_yaml::Value::Mapping(Default::default())
    } else {
        serde_yaml::from_str(fm_str).map_err(|e| anyhow::anyhow!("frontmatter parse error: {e}"))?
    };

    if let serde_yaml::Value::Mapping(ref mut map) = yaml {
        if let Some(serde_yaml::Value::Sequence(ref mut seq)) =
            map.get_mut(serde_yaml::Value::String(key.to_string()))
        {
            seq.retain(|v| v.as_str().map(|s| s != value).unwrap_or(true));
        }
    }

    let new_fm = serde_yaml::to_string(&yaml)
        .map_err(|e| anyhow::anyhow!("frontmatter serialize error: {e}"))?;
    if body.is_empty() {
        Ok(format!("---\n{}---\n", new_fm))
    } else {
        Ok(format!("---\n{}---\n{}", new_fm, body))
    }
}

// -----------------------------------------------------------------------------
// tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gait_state::proto::gait::log::v1::{event, payload};
    use gait_state::session::log::read_events;
    use tempfile::TempDir;

    // --- helpers ---

    fn ws_with_skill_and_session() -> (TempDir, String) {
        let ws = TempDir::new().unwrap();
        let session_id = "load0001".to_string();
        std::fs::create_dir_all(ws.path().join("skills/tdd")).unwrap();
        std::fs::write(
            ws.path().join("skills/tdd/SKILL.md"),
            "---\nname: tdd\ndescription: x\nenabled: false\n---\n# body\n",
        )
        .unwrap();
        let tx_path = gait_state::session::live::log_path(ws.path(), &session_id);
        std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();
        (ws, session_id)
    }

    fn read_toggle(tx_path: &Path) -> gait_state::proto::gait::log::v1::Toggle {
        let events = read_events(tx_path).unwrap();
        match &events[0].body {
            Some(event::Body::Payload(p)) => match &p.kind {
                Some(payload::Kind::Toggle(t)) => t.clone(),
                other => panic!("expected Toggle payload, got {other:?}"),
            },
            other => panic!("expected payload body, got {other:?}"),
        }
    }

    // -- load / unload --

    #[test]
    fn load_appends_toggle_enabled_true_to_quiescent_log() {
        let (ws, sid) = ws_with_skill_and_session();
        let tx_path = gait_state::session::live::log_path(ws.path(), &sid);
        load(LoadParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: sid.clone(),
            kind: ExtKind::Skill,
            name: "tdd".into(),
        })
        .unwrap();
        let toggle = read_toggle(&tx_path);
        assert_eq!(toggle.name, "tdd");
        assert!(toggle.enabled, "load must set enabled=true");
    }

    #[test]
    fn unload_appends_toggle_enabled_false_to_quiescent_log() {
        let (ws, sid) = ws_with_skill_and_session();
        let tx_path = gait_state::session::live::log_path(ws.path(), &sid);
        unload(LoadParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: sid.clone(),
            kind: ExtKind::Skill,
            name: "tdd".into(),
        })
        .unwrap();
        let toggle = read_toggle(&tx_path);
        assert_eq!(toggle.name, "tdd");
        assert!(!toggle.enabled, "unload must set enabled=false");
    }

    #[test]
    fn resolve_load_targets_rejects_unknown_name() {
        let ws = TempDir::new().unwrap();
        let err =
            resolve_load_targets(ExtKind::Skill, "no-such-skill", ws.path(), "anyid").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("not registered"),
            "expected 'not registered' error, got: {msg}"
        );
    }

    // -- register / unregister / enable / disable --

    fn reg(kind: &str, name: &str, dir: &Path) -> RegisterParams {
        RegisterParams {
            gait_dir: dir.to_path_buf(),
            kind: kind.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    fn unreg(kind: &str, name: &str, dir: &Path) -> UnregisterParams {
        UnregisterParams {
            gait_dir: dir.to_path_buf(),
            kind: kind.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    fn en(kind: &str, name: &str, dir: &Path) -> EnableParams {
        EnableParams {
            gait_dir: dir.to_path_buf(),
            kind: kind.into(),
            name: name.into(),
        }
    }

    #[test]
    fn register_mcp_writes_entry_disabled() {
        let dir = TempDir::new().unwrap();
        let mut p = reg("mcp", "slack", dir.path());
        p.spawn = Some("node mcp-slack.js".into());
        register(p).unwrap();
        let val: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("mcp").join("slack.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(val["command"], "node");
        assert_eq!(val["args"][0], "mcp-slack.js");
        assert_eq!(val["enabled"], false, "load should default to disabled");
    }

    #[test]
    fn register_mcp_writes_disk_file_via_command_args() {
        let dir = TempDir::new().unwrap();
        let mut p = reg("mcp", "slack", dir.path());
        p.command = Some("npx".into());
        p.args = Some("-y @pkg/slack".into());
        register(p).unwrap();
        let disk_path = dir.path().join("mcp").join("slack.json");
        assert!(disk_path.exists());
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&disk_path).unwrap()).unwrap();
        assert_eq!(val["command"], "npx");
        assert_eq!(val["enabled"], false);
    }

    #[test]
    fn register_skill_writes_entry() {
        let dir = TempDir::new().unwrap();
        let mut p = reg("skill", "tdd", dir.path());
        p.description = Some("tdd workflow".into());
        p.content = Some("# TDD".into());
        register(p).unwrap();
        let skill_md = dir.path().join("skills").join("tdd").join("SKILL.md");
        assert!(skill_md.exists());
        let md = std::fs::read_to_string(&skill_md).unwrap();
        assert!(md.contains("name: tdd"));
        assert!(md.contains("description: tdd workflow"));
        assert!(md.contains("enabled: false"));
        assert!(md.contains("# TDD"));
    }

    #[test]
    fn register_plugin_creates_directory_and_md() {
        let dir = TempDir::new().unwrap();
        let wasm_path = dir.path().join("myfmt.wasm");
        std::fs::write(&wasm_path, b"").unwrap();
        let mut p = reg("plugin", "myfmt", dir.path());
        p.wasm = Some(wasm_path.to_str().unwrap().to_string());
        p.description = Some("A formatter".into());
        register(p).unwrap();
        let plugin_dir = dir.path().join("plugins").join("myfmt");
        assert!(plugin_dir.is_dir());
        assert!(plugin_dir.join("myfmt.wasm").exists());
        let md = std::fs::read_to_string(plugin_dir.join("PLUGIN.md")).unwrap();
        assert!(md.contains("name: myfmt"));
        assert!(md.contains("enabled: false"));
        assert!(md.contains("A formatter"));
    }

    #[test]
    fn register_plugin_missing_wasm_errors() {
        let dir = TempDir::new().unwrap();
        assert!(register(reg("plugin", "foo", dir.path())).is_err());
    }

    #[test]
    fn unregister_skill_removes_directory() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("tdd");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: tdd\ndescription: x\nenabled: false\n---\n",
        )
        .unwrap();
        unregister(unreg("skill", "tdd", dir.path())).unwrap();
        assert!(!skill_dir.exists());
    }

    #[test]
    fn unregister_mcp_removes_disk_file() {
        let dir = TempDir::new().unwrap();
        let mcp_dir = dir.path().join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        let disk_path = mcp_dir.join("slack.json");
        std::fs::write(&disk_path, r#"{"command":"npx","args":[],"enabled":false}"#).unwrap();
        unregister(unreg("mcp", "slack", dir.path())).unwrap();
        assert!(!disk_path.exists());
    }

    #[test]
    fn unregister_plugin_removes_directory() {
        let dir = TempDir::new().unwrap();
        let plugin_dir = dir.path().join("plugins").join("myfmt");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("PLUGIN.md"),
            "---\nname: myfmt\nwasm: myfmt.wasm\nenabled: false\n---\n",
        )
        .unwrap();
        unregister(unreg("plugin", "myfmt", dir.path())).unwrap();
        assert!(!plugin_dir.exists());
    }

    #[test]
    fn unregister_removes_legacy_config_entry() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[{"name":"slack","spawn":"node s.js","enabled":true}]}"#,
        )
        .unwrap();
        unregister(unreg("mcp", "slack", dir.path())).unwrap();
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap())
                .unwrap();
        assert!(val["mcp_servers"].as_array().unwrap().is_empty());
    }

    fn write_agent(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("agent.md");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn register_scope_agent_appends_to_agent_frontmatter() {
        let dir = TempDir::new().unwrap();
        let agent_path = write_agent(
            dir.path(),
            "---\nname: myagent\nallow:\n  - bash\n---\n# body\n",
        );
        let mut p = reg("skill", "tdd", dir.path());
        p.scope = Some("agent".into());
        p.agent = Some(agent_path.clone());
        register(p).unwrap();
        let md = std::fs::read_to_string(&agent_path).unwrap();
        assert!(md.contains("- tdd"));
        assert!(md.contains("allow:"));
        assert!(md.contains("- bash"));
        assert!(md.contains("# body"));
        assert!(!dir.path().join("skills").exists());
    }

    #[test]
    fn unregister_scope_agent_removes_from_agent_frontmatter() {
        let dir = TempDir::new().unwrap();
        let agent_path = write_agent(
            dir.path(),
            "---\nname: myagent\nskills:\n  - tdd\n---\n# body\n",
        );
        let mut p = unreg("skill", "tdd", dir.path());
        p.scope = Some("agent".into());
        p.agent = Some(agent_path.clone());
        unregister(p).unwrap();
        let md = std::fs::read_to_string(&agent_path).unwrap();
        assert!(!md.contains("- tdd"));
        assert!(md.contains("# body"));
    }

    #[test]
    fn register_scope_agent_requires_agent_path() {
        let dir = TempDir::new().unwrap();
        let mut p = reg("skill", "tdd", dir.path());
        p.scope = Some("agent".into());
        let err = register(p).unwrap_err();
        assert!(format!("{err}").contains("--agent"));
    }

    /// Create a quiescent (no lock file) live session's log directory, empty
    /// transcript, under `gait_dir` — the minimal shape `write_registry` needs.
    fn make_quiescent_session(gait_dir: &Path, session_id: &str) {
        let tx_path = gait_state::session::live::log_path(gait_dir, session_id);
        std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn register_scope_session_appends_registry_event() {
        let dir = TempDir::new().unwrap();
        let session_id = "sabc123";
        make_quiescent_session(dir.path(), session_id);
        let mut p = reg("skill", "tdd", dir.path());
        p.scope = Some("session".into());
        p.session = Some(session_id.into());
        register(p).unwrap();

        let tx_path = gait_state::session::live::log_path(dir.path(), session_id);
        let events = read_events(&tx_path).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0].body {
            Some(event::Body::Payload(pl)) => match &pl.kind {
                Some(payload::Kind::Registry(r)) => {
                    assert_eq!(r.name, "tdd");
                    assert!(r.listed);
                }
                other => panic!("expected Registry event, got {other:?}"),
            },
            other => panic!("expected payload body, got {other:?}"),
        }
        assert!(!dir.path().join("skills").exists());
    }

    #[test]
    fn unregister_scope_session_appends_unlisted_registry_event() {
        let dir = TempDir::new().unwrap();
        let session_id = "sabc123";
        make_quiescent_session(dir.path(), session_id);
        let mut p = unreg("skill", "tdd", dir.path());
        p.scope = Some("session".into());
        p.session = Some(session_id.into());
        unregister(p).unwrap();

        let tx_path = gait_state::session::live::log_path(dir.path(), session_id);
        let events = read_events(&tx_path).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0].body {
            Some(event::Body::Payload(pl)) => match &pl.kind {
                Some(payload::Kind::Registry(r)) => {
                    assert_eq!(r.name, "tdd");
                    assert!(!r.listed);
                }
                other => panic!("expected Registry event, got {other:?}"),
            },
            other => panic!("expected payload body, got {other:?}"),
        }
    }

    #[test]
    fn register_scope_session_requires_session_id() {
        let dir = TempDir::new().unwrap();
        let mut p = reg("skill", "tdd", dir.path());
        p.scope = Some("session".into());
        let err = register(p).unwrap_err();
        assert!(format!("{err}").contains("--session"));
    }

    #[test]
    fn enable_skill_prefers_disk_file() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("tdd");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: tdd\ndescription: x\nenabled: false\n---\n# body\n",
        )
        .unwrap();
        enable(en("skill", "tdd", dir.path())).unwrap();
        let md = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(md.contains("enabled: true"));
        assert!(!md.contains("enabled: false"));
    }

    #[test]
    fn enable_mcp_prefers_disk_file() {
        let dir = TempDir::new().unwrap();
        let mcp_dir = dir.path().join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(
            mcp_dir.join("slack.json"),
            r#"{"command":"npx","args":[],"enabled":false}"#,
        )
        .unwrap();
        enable(en("mcp", "slack", dir.path())).unwrap();
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(mcp_dir.join("slack.json")).unwrap())
                .unwrap();
        assert_eq!(val["enabled"], true);
    }

    #[test]
    fn enable_plugin_sets_flag_in_plugin_md() {
        let dir = TempDir::new().unwrap();
        let plugin_dir = dir.path().join("plugins").join("myfmt");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("PLUGIN.md"),
            "---\nname: myfmt\nwasm: myfmt.wasm\nenabled: false\n---\n# docs\n",
        )
        .unwrap();
        enable(en("plugin", "myfmt", dir.path())).unwrap();
        let md = std::fs::read_to_string(plugin_dir.join("PLUGIN.md")).unwrap();
        assert!(md.contains("enabled: true"));
        assert!(!md.contains("enabled: false"));
    }

    #[test]
    fn enable_sets_flag_in_legacy_config() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[{"name":"slack","spawn":"node s.js","enabled":false}]}"#,
        )
        .unwrap();
        enable(en("mcp", "slack", dir.path())).unwrap();
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap())
                .unwrap();
        assert_eq!(val["mcp_servers"][0]["enabled"], true);
    }

    #[test]
    fn disable_sets_flag_in_legacy_config() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[{"name":"slack","spawn":"node s.js","enabled":true}]}"#,
        )
        .unwrap();
        disable(en("mcp", "slack", dir.path())).unwrap();
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap())
                .unwrap();
        assert_eq!(val["mcp_servers"][0]["enabled"], false);
    }

    #[test]
    fn enable_missing_entry_errors() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[]}"#,
        )
        .unwrap();
        assert!(enable(en("mcp", "slack", dir.path())).is_err());
    }

    #[test]
    fn min_register_load_commands_are_unconditional() {
        // `gait-ops` has no `min`/`full` feature split at all (unlike
        // `gait-core`/`gait-bin`, which gate `rmcp`/`gait_codemode` behind
        // `mcp`/`code-mode`) — that absence is itself the round-6 finding
        // made concrete: there is nothing here to gate. `register`/`load`
        // with kind="mcp" write/read the same raw JSON file and Toggle
        // event a full build would, regardless of whether `gait-core`'s
        // `mcp`/`code-mode` features are compiled in. Only turn-execution
        // time (actually opening a live MCP connection) differs under
        // `min` — untouched by this task and not exercised here.
        let dir = TempDir::new().unwrap();
        let session_id = "minmcp01";
        let tx_path = gait_state::session::live::log_path(dir.path(), session_id);
        std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();

        // register mcp — writes mcp/<name>.json, disabled by default.
        let mut p = reg("mcp", "slack", dir.path());
        p.spawn = Some("node mcp-slack.js".into());
        let outcome = register(p).unwrap();
        assert_eq!(outcome.scope, "global");
        let disk_path = dir.path().join("mcp").join("slack.json");
        assert!(disk_path.exists());
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&disk_path).unwrap()).unwrap();
        assert_eq!(val["command"], "node");
        assert_eq!(val["enabled"], false);

        // load mcp — appends a Toggle(enabled=true) event to the session
        // log, the same bookkeeping act a full build performs.
        let load_outcome = load(LoadParams {
            gait_dir: dir.path().to_path_buf(),
            session_id: session_id.to_string(),
            kind: ExtKind::Mcp,
            name: "slack".to_string(),
        })
        .unwrap();
        assert!(load_outcome.enabled);
        let toggle = read_toggle(&tx_path);
        assert_eq!(toggle.name, "slack");
        assert!(toggle.enabled, "load must set enabled=true even under min");

        // unload mcp — appends a Toggle(enabled=false) event; still no
        // feature gate involved.
        let unload_outcome = unload(LoadParams {
            gait_dir: dir.path().to_path_buf(),
            session_id: session_id.to_string(),
            kind: ExtKind::Mcp,
            name: "slack".to_string(),
        })
        .unwrap();
        assert!(!unload_outcome.enabled);

        // unregister mcp — removes the disk entry. Still no feature gate.
        let unreg_outcome = unregister(unreg("mcp", "slack", dir.path())).unwrap();
        assert_eq!(unreg_outcome.scope, "global");
        assert!(!disk_path.exists());
    }

    #[test]
    fn load_is_visible_to_the_next_turns_agent_state_projection() {
        // This is the exact fold turn-start uses to compute the active
        // skill/mcp/plugin set for the system prompt (gait_state::session::
        // retrace::ActiveAgent, driven by run_projection). Proving `load`'s
        // Toggle event round-trips through THIS fold — the same fold every
        // turn (REPL or CLI) runs at start — proves `gait load ext --session
        // <id>` followed by a NEW `gait "task" --session <id>` invocation sees
        // the loaded extension exactly like the REPL's `/skill load` does
        // inline. Two commands instead of one REPL line, same eventual state.
        use gait_state::session::retrace::{ActiveAgent, AgentState};
        use std::collections::HashMap;

        let (ws, sid) = ws_with_skill_and_session();
        let tx_path = gait_state::session::live::log_path(ws.path(), &sid);

        load(LoadParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: sid.clone(),
            kind: ExtKind::Skill,
            name: "tdd".to_string(),
        })
        .unwrap();

        let events = read_events(&tx_path).unwrap();
        let agent = gait_state::session::retrace::run_projection(
            &ActiveAgent {
                seed: AgentState::default(),
            },
            &events,
            &HashMap::new(),
        );
        assert!(
            agent.skills.contains("tdd"),
            "load's Toggle event must be visible to the same AgentState projection every turn folds"
        );
    }
}
