//! Registry/config administration + auth + models.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

use crate::cli::error::{CliError, CliResult};
use clap::Parser;
use std::path::PathBuf;

/// `gait inference list|show|select|clear [name] --session <id>`.
#[derive(Parser, Debug)]
pub struct InferenceArgs {
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    pub action: Option<String>,
    pub name: Option<String>,
    /// Model for the `add` action, in `provider/id` form (e.g. `opencode/kimi-k2.6`).
    /// Ignored by other actions.
    #[arg(long)]
    pub model: Option<String>,
    /// Required compaction threshold for the `add` action.
    #[arg(long)]
    pub compaction_threshold_tokens: Option<u64>,
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long)]
    pub pretty: bool,
}

/// `gait local setup --model-path /path/to/model.gguf`
#[cfg(feature = "local")]
#[derive(Parser, Debug)]
pub struct LocalArgs {
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    /// Action: setup (default). More local profiles can be added without
    /// changing the top-level command later.
    pub action: Option<String>,
    /// Named inference to create. Refuses to overwrite an existing profile.
    #[arg(long, default_value = "local-qwen")]
    pub name: String,
    /// Developer-owned GGUF for Qwen2.5-Coder-7B-Instruct Q4_K_M.
    #[arg(long)]
    pub model_path: Option<PathBuf>,
    /// Context window to give llama.cpp. 8192 is a conservative Intel-safe
    /// starter; the model itself supports more when memory permits.
    #[arg(long, default_value_t = 8192)]
    pub context_tokens: u32,
    /// llama.cpp layers to offload. 0 is CPU-only and works on Intel Macs.
    #[arg(long, default_value_t = 0)]
    pub gpu_layers: u32,
    /// Begin compaction before the context is full.
    #[arg(long, default_value_t = 6000)]
    pub compaction_threshold_tokens: u64,
    #[arg(long)]
    pub pretty: bool,
}

#[cfg(feature = "local")]
pub async fn cmd_local(args: LocalArgs) -> CliResult {
    match args.action.as_deref().unwrap_or("setup") {
        "setup" => {
            let model_path = args.model_path.as_deref().ok_or_else(|| {
                CliError::usage("[gait] usage: gait local setup --model-path /path/to/model.gguf")
            })?;
            match gait_ops::inference::add_local_qwen(
                &args.gait_dir,
                &args.name,
                model_path,
                args.context_tokens,
                args.gpu_layers,
                args.compaction_threshold_tokens,
            ) {
                Ok(inference) => println!(
                    "{}",
                    gait_ops::render_json(
                        &serde_json::json!({
                            "name": inference.name,
                            "model": inference.model,
                            "gguf": model_path.display().to_string(),
                            "action": "added",
                        }),
                        args.pretty
                    )
                ),
                Err(error) => return Err(CliError::failed_after("[gait]", error)),
            }
        }
        "status" | "stop" => {
            let inference = match gait_core::inferences::load_inference(&args.gait_dir, &args.name)
            {
                Ok(value) => value,
                Err(error) => return Err(CliError::failed_after("[gait]", error)),
            };
            let params = match inference
                .llm_params
                .as_ref()
                .map(gait_core::proto::events::llm_params_from_proto)
                .transpose()
            {
                Ok(Some(value)) => value,
                _ => {
                    return Err(CliError::invalid_input(format!(
                        "[gait] inference '{}' is not a configured local profile",
                        args.name
                    )))
                }
            };
            let Some(local) = params.local else {
                return Err(CliError::invalid_input(format!(
                    "[gait] inference '{}' is not a configured local profile",
                    args.name
                )));
            };
            let protocol = if inference.model == "local/qwen2.5-coder-7b-instruct-q4-k-m" {
                "qwen25-coder"
            } else {
                "unsupported"
            };
            let key = gait_local::runner_key(&format!(
                "model={};ctx={};gpu={:?};tool={protocol}",
                local.model_path,
                local.context_tokens.unwrap_or(32_768),
                local.gpu_layers.or(Some(0))
            ));
            let socket = args
                .gait_dir
                .join("local-runners")
                .join(format!("{key}.sock"));
            let render = |status: &str, model_key: Option<&str>| {
                println!(
                    "{}",
                    gait_ops::render_json(
                        &serde_json::json!({"status": status, "model_key": model_key}),
                        args.pretty
                    )
                )
            };
            match local_runner_command(&socket, args.action.as_deref().unwrap()).await {
                Ok(Some(gait_local::RunnerResponse::Status { model_key })) => {
                    render("running", Some(&model_key))
                }
                Ok(Some(gait_local::RunnerResponse::Stopped)) => render("stopped", None),
                Ok(_) => render("not running", None),
                Err(_) if args.action.as_deref() == Some("status") => render("not running", None),
                Err(error) => {
                    return Err(CliError::failed(format!("[gait] local runner: {error}")))
                }
            }
        }
        _ => {
            return Err(CliError::usage(
                "[gait] local action must be setup, status, or stop",
            ))
        }
    }
    Ok(())
}

#[cfg(feature = "local")]
async fn local_runner_command(
    socket: &std::path::Path,
    action: &str,
) -> Result<Option<gait_local::RunnerResponse>, std::io::Error> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let request = if action == "stop" {
        gait_local::RunnerRequest::Stop
    } else {
        gait_local::RunnerRequest::Status
    };
    stream
        .write_all(
            serde_json::to_string(&request)
                .expect("runner request serializes")
                .as_bytes(),
        )
        .await?;
    stream.write_all(b"\n").await?;
    let mut lines = tokio::io::BufReader::new(stream).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(None);
    };
    Ok(serde_json::from_str(&line).ok())
}

pub fn cmd_inference(args: InferenceArgs) -> CliResult {
    let pretty = args.pretty;
    match args.action.as_deref().unwrap_or("list") {
        "list" => {
            let names: Vec<String> = gait_ops::inference::list(&args.gait_dir);
            println!(
                "{}",
                gait_ops::render_json(&serde_json::json!({"inferences": names}), pretty)
            );
        }
        "show" => {
            let name = args
                .name
                .as_deref()
                .ok_or_else(|| CliError::usage("[gait] usage: gait inference show <name>"))?;
            match gait_ops::inference::show(&args.gait_dir, name) {
                Ok(i) => println!(
                    "{}",
                    gait_ops::render_json(
                        &serde_json::json!({"name": i.name, "model": i.model}),
                        pretty
                    )
                ),
                Err(e) => return Err(CliError::failed_after("[gait]", e)),
            }
        }
        "select" => {
            let name = args.name.as_deref().ok_or_else(|| {
                CliError::usage("[gait] usage: gait inference select <name> --session <id>")
            })?;
            let session = args
                .session
                .as_deref()
                .ok_or_else(|| CliError::usage("[gait] --session is required"))?;
            (|| -> CliResult {
                let worktree = gait_core::session::live::worktree_dir(&args.gait_dir, session);
                if !worktree.exists() {
                    return Err(CliError::not_found(format!(
                        "[gait] session '{session}' is not active"
                    )));
                }
                let selection = gait_ops::inference::resolve_selection(&args.gait_dir, name)
                    .map_err(|e| CliError::failed_after("[gait]", e))?;
                gait_core::embed::record_session_update(
                    &gait_core::git::Repos::production(),
                    &args.gait_dir,
                    session,
                    vec![selection.event_body],
                )
                .map_err(|e| CliError::failed_after("[gait]", e))
            })()?;
        }
        "clear" => {
            let session = args
                .session
                .as_deref()
                .ok_or_else(|| CliError::usage("[gait] --session is required"))?;
            (|| -> CliResult {
                let worktree = gait_core::session::live::worktree_dir(&args.gait_dir, session);
                if !worktree.exists() {
                    return Err(CliError::not_found(format!(
                        "[gait] session '{session}' is not active"
                    )));
                }
                let selection = gait_ops::inference::resolve_selection(&args.gait_dir, "default")
                    .map_err(|e| CliError::failed_after("[gait]", e))?;
                gait_core::embed::record_session_update(
                    &gait_core::git::Repos::production(),
                    &args.gait_dir,
                    session,
                    vec![selection.event_body],
                )
                .map_err(|e| CliError::failed_after("[gait]", e))
            })()?;
        }
        "add" => {
            let name = args.name.as_deref().ok_or_else(|| {
                CliError::usage("[gait] usage: gait inference add <name> --model <provider/id>")
            })?;
            let model = args.model.as_deref().ok_or_else(|| {
                CliError::usage("[gait] usage: gait inference add <name> --model <provider/id>")
            })?;
            let compaction_threshold_tokens = args.compaction_threshold_tokens.ok_or_else(|| {
                CliError::usage("[gait] --compaction-threshold-tokens is required for inference add")
            })?;
            match gait_ops::inference::add(&args.gait_dir, name, model, compaction_threshold_tokens)
            {
                Ok(i) => println!(
                    "{}",
                    gait_ops::render_json(
                        &serde_json::json!({"name": i.name, "model": i.model, "action": "added"}),
                        pretty
                    )
                ),
                Err(e) => return Err(CliError::failed_after("[gait]", e)),
            }
        }
        _ => {
            return Err(CliError::usage(
                "[gait] inference action must be list, show, select, clear, or add",
            ))
        }
    }
    Ok(())
}

/// `gait mode list|show|add|select|clear [name] --session <id>`.
#[derive(Parser, Debug)]
pub struct ModeArgs {
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    pub action: Option<String>,
    pub name: Option<String>,
    /// Repeatable allow pattern for the `add` action. Ignored by other actions.
    #[arg(long = "allow")]
    pub allow: Vec<String>,
    /// Repeatable deny pattern for the `add` action. Ignored by other actions.
    #[arg(long = "deny")]
    pub deny: Vec<String>,
    /// Freeform steering text for the `add` action. Ignored by other actions.
    #[arg(long, default_value = "")]
    pub steer: String,
    /// Human-readable description for the `add` action. Ignored by other actions.
    #[arg(long, default_value = "")]
    pub description: String,
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long)]
    pub pretty: bool,
}

pub fn cmd_mode(args: ModeArgs) -> CliResult {
    let pretty = args.pretty;
    match args.action.as_deref().unwrap_or("list") {
        "list" => {
            let names: Vec<String> = gait_ops::mode::list(&args.gait_dir);
            println!(
                "{}",
                gait_ops::render_json(&serde_json::json!({"modes": names}), pretty)
            );
        }
        "show" => {
            let name = args
                .name
                .as_deref()
                .ok_or_else(|| CliError::usage("[gait] usage: gait mode show <name>"))?;
            match gait_ops::mode::show(&args.gait_dir, name) {
                Ok(m) => println!(
                    "{}",
                    gait_ops::render_json(
                        &serde_json::json!({
                            "name": m.name,
                            "allow": m.allow,
                            "deny": m.deny,
                            "steer": m.steer,
                            "description": m.description,
                        }),
                        pretty
                    )
                ),
                Err(e) => return Err(CliError::failed_after("[gait]", e)),
            }
        }
        "select" => {
            let name = args.name.as_deref().ok_or_else(|| {
                CliError::usage("[gait] usage: gait mode select <name> --session <id>")
            })?;
            let session = args
                .session
                .as_deref()
                .ok_or_else(|| CliError::usage("[gait] --session is required"))?;
            (|| -> CliResult {
                let worktree = gait_core::session::live::worktree_dir(&args.gait_dir, session);
                if !worktree.exists() {
                    return Err(CliError::not_found(format!(
                        "[gait] session '{session}' is not active"
                    )));
                }
                let selection = gait_ops::mode::resolve_selection(&args.gait_dir, name)
                    .map_err(|e| CliError::failed_after("[gait]", e))?;
                gait_core::embed::record_session_update(
                    &gait_core::git::Repos::production(),
                    &args.gait_dir,
                    session,
                    vec![selection.event_body],
                )
                .map_err(|e| CliError::failed_after("[gait]", e))
            })()?;
        }
        "clear" => {
            let session = args
                .session
                .as_deref()
                .ok_or_else(|| CliError::usage("[gait] --session is required"))?;
            (|| -> CliResult {
                let worktree = gait_core::session::live::worktree_dir(&args.gait_dir, session);
                if !worktree.exists() {
                    return Err(CliError::not_found(format!(
                        "[gait] session '{session}' is not active"
                    )));
                }
                let selection = gait_ops::mode::resolve_selection(&args.gait_dir, "default")
                    .map_err(|e| CliError::failed_after("[gait]", e))?;
                gait_core::embed::record_session_update(
                    &gait_core::git::Repos::production(),
                    &args.gait_dir,
                    session,
                    vec![selection.event_body],
                )
                .map_err(|e| CliError::failed_after("[gait]", e))
            })()?;
        }
        "add" => {
            let name = args.name.as_deref().ok_or_else(|| {
                CliError::usage("[gait] usage: gait mode add <name> [--allow <pattern>]... [--deny <pattern>]... [--steer <text>] [--description <text>]")
            })?;
            match gait_ops::mode::add(
                &args.gait_dir,
                name,
                &args.allow,
                &args.deny,
                &args.steer,
                &args.description,
            ) {
                Ok(m) => println!(
                    "{}",
                    gait_ops::render_json(
                        &serde_json::json!({
                            "name": m.name,
                            "allow": m.allow,
                            "deny": m.deny,
                            "action": "added",
                        }),
                        pretty
                    )
                ),
                Err(e) => return Err(CliError::failed_after("[gait]", e)),
            }
        }
        _ => {
            return Err(CliError::usage(
                "[gait] mode action must be list, show, add, select, or clear",
            ))
        }
    }
    Ok(())
}

/// `gait register mcp|skill|plugin <name> [flags]`
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct RegisterArgs {
    /// Kind: mcp | skill | plugin
    pub kind: String,
    /// Extension name
    pub name: String,
    /// MCP URL (mcp kind)
    #[arg(long)]
    pub url: Option<String>,
    /// MCP command program (mcp kind)
    #[arg(long)]
    pub command: Option<String>,
    /// MCP args string (mcp kind)
    #[arg(long)]
    pub args: Option<String>,
    /// MCP compat spawn string (mcp kind)
    #[arg(long)]
    pub spawn: Option<String>,
    /// MCP compat URL flag (mcp kind)
    #[arg(long)]
    pub connect: Option<String>,
    /// Skill description (skill kind)
    #[arg(long)]
    pub description: Option<String>,
    /// Skill content (skill kind)
    #[arg(long)]
    pub content: Option<String>,
    /// Skill path (skill kind)
    #[arg(long)]
    pub path: Option<String>,
    /// Plugin wasm source path (plugin kind)
    #[arg(long)]
    pub wasm: Option<String>,
    /// Extra env vars (repeated)
    #[arg(long, action = clap::ArgAction::Append)]
    pub env: Vec<String>,
    /// Skip confirmation prompts
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Gait dir (falls back to $GAIT_DIR)
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: Option<PathBuf>,
    /// Scope: global | agent | session (default: global)
    #[arg(long)]
    pub scope: Option<String>,
    /// Path to agent .md file (only meaningful with --scope agent)
    #[arg(long)]
    pub agent: Option<PathBuf>,
    /// Session id (only meaningful with --scope session)
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long)]
    pub pretty: bool,
}

/// `gait unregister mcp|skill|plugin <name>`
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct UnregisterArgs {
    /// Kind: mcp | skill | plugin
    pub kind: String,
    /// Extension name
    pub name: String,
    /// Gait dir (falls back to $GAIT_DIR)
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: Option<PathBuf>,
    /// Scope: global | agent | session (default: global)
    #[arg(long)]
    pub scope: Option<String>,
    /// Path to agent .md file (only meaningful with --scope agent)
    #[arg(long)]
    pub agent: Option<PathBuf>,
    /// Session id (only meaningful with --scope session)
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long)]
    pub pretty: bool,
}

/// `gait enable mcp|skill|plugin <name>`
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct EnableArgs {
    /// Kind: mcp | skill | plugin
    pub kind: String,
    /// Extension name
    pub name: String,
    /// Gait dir (falls back to $GAIT_DIR)
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: Option<PathBuf>,
    #[arg(long)]
    pub pretty: bool,
}

/// `gait disable mcp|skill|plugin <name>`
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct DisableArgs {
    /// Kind: mcp | skill | plugin
    pub kind: String,
    /// Extension name
    pub name: String,
    /// Gait dir (falls back to $GAIT_DIR)
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: Option<PathBuf>,
    #[arg(long)]
    pub pretty: bool,
}
/// Parse --key value pairs from an arg slice into a HashMap.
pub fn parse_flags(args: &[String]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(key) = args[i].strip_prefix("--") {
            if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                map.insert(key.to_string(), args[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    map
}

pub fn cmd_register(args: RegisterArgs) -> anyhow::Result<()> {
    let gait_dir = crate::cli::args::resolve_gait_dir_from(args.gait_dir.clone())?;

    let params = gait_ops::extension::RegisterParams {
        gait_dir,
        kind: args.kind.clone(),
        name: args.name.clone(),
        url: args.url.clone(),
        command: args.command.clone(),
        args: args.args.clone(),
        spawn: args.spawn.clone(),
        connect: args.connect.clone(),
        description: args.description.clone(),
        content: args.content.clone(),
        path: args.path.clone(),
        wasm: args.wasm.clone(),
        scope: args.scope.clone(),
        agent: args.agent.clone(),
        session: args.session.clone(),
    };

    let out = gait_ops::extension::register(params)?;
    println!(
        "{}",
        gait_ops::render_json(
            &serde_json::json!({
                "kind": args.kind,
                "name": args.name,
                "scope": out.scope,
                "written_to": out.written_to.display().to_string(),
                "session_id": args.session,
                "hint": out.hint,
                "action": "registered",
            }),
            args.pretty
        )
    );
    Ok(())
}

pub fn cmd_enable(args: EnableArgs) -> CliResult {
    let gait_dir = crate::cli::args::resolve_gait_dir_from(args.gait_dir.clone())
        .map_err(|error| CliError::failed_after("[gait]", error))?;
    let params = gait_ops::extension::EnableParams {
        gait_dir,
        kind: args.kind.clone(),
        name: args.name.clone(),
    };
    match gait_ops::extension::enable(params) {
        Ok(_) => println!(
            "{}",
            gait_ops::render_json(
                &serde_json::json!({"kind": args.kind, "name": args.name, "action": "enabled"}),
                args.pretty
            )
        ),
        Err(e) => return Err(CliError::failed_with("[gait] error", e)),
    }
    Ok(())
}

pub fn cmd_disable(args: DisableArgs) -> CliResult {
    let gait_dir = crate::cli::args::resolve_gait_dir_from(args.gait_dir.clone())
        .map_err(|error| CliError::failed_after("[gait]", error))?;
    let params = gait_ops::extension::EnableParams {
        gait_dir,
        kind: args.kind.clone(),
        name: args.name.clone(),
    };
    match gait_ops::extension::disable(params) {
        Ok(_) => println!(
            "{}",
            gait_ops::render_json(
                &serde_json::json!({"kind": args.kind, "name": args.name, "action": "disabled"}),
                args.pretty
            )
        ),
        Err(e) => return Err(CliError::failed_with("[gait] error", e)),
    }
    Ok(())
}

pub fn cmd_unregister(args: UnregisterArgs) -> anyhow::Result<()> {
    let gait_dir = crate::cli::args::resolve_gait_dir_from(args.gait_dir.clone())?;

    let params = gait_ops::extension::UnregisterParams {
        gait_dir,
        kind: args.kind.clone(),
        name: args.name.clone(),
        scope: args.scope.clone(),
        agent: args.agent.clone(),
        session: args.session.clone(),
    };

    let out = gait_ops::extension::unregister(params)?;
    println!(
        "{}",
        gait_ops::render_json(
            &serde_json::json!({
                "kind": args.kind,
                "name": args.name,
                "scope": out.scope,
                "written_to": out.written_to.display().to_string(),
                "session_id": args.session,
                "action": "unregistered",
            }),
            args.pretty
        )
    );
    Ok(())
}

/// Parse a `gait config set` value: try JSON first (numbers, bools, arrays,
/// objects), fall back to a raw string (so `anthropic/claude-opus-4` doesn't
/// need quoting) — matches the existing ergonomic of the generic `set`.
fn parse_config_value(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

/// `gait config <set|get|show> ...` — the general external-CLI equivalent of
/// any REPL slash command that reads or mutates `gait.jsonc`. `set`/`get`
/// call the generic `gait_ops::config::set`/`get` (the comment-safe jsonc
/// mutators); `show` calls the shared `gait_ops::config::resolve` op.
/// gait-dir-wide, not session-scoped. Mirrors `WorkflowArgs`/`cmd_workflow`'s
/// action+rest shape exactly.
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct ConfigArgs {
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    /// Action: set | get | show
    pub action: Option<String>,
    /// Remaining args: `<key> <value>` for `set`, `<key>` for `get`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
    #[arg(long)]
    pub pretty: bool,
}

pub fn cmd_config(args: ConfigArgs) -> CliResult {
    let sub = args.action.as_deref().unwrap_or("");
    match sub {
        "set" => {
            let key = args.rest.first().cloned().ok_or_else(|| {
                CliError::usage("[gait] usage: gait config set <key> <value>")
            })?;
            let value_raw = args.rest.get(1).cloned().ok_or_else(|| {
                CliError::usage("[gait] usage: gait config set <key> <value>")
            })?;
            let value = parse_config_value(&value_raw);
            match gait_ops::config::set(&args.gait_dir, &key, value) {
                Ok(()) => println!(
                    "{}",
                    gait_ops::render_json(
                        &serde_json::json!({"key": key, "value": value_raw}),
                        args.pretty
                    )
                ),
                Err(e) => return Err(CliError::failed_with("[gait] error", e)),
            }
        }
        "get" => {
            let key = args
                .rest
                .first()
                .cloned()
                .ok_or_else(|| CliError::usage("[gait] usage: gait config get <key>"))?;
            match gait_ops::config::get(&args.gait_dir, &key) {
                Ok(v) => println!(
                    "{}",
                    gait_ops::render_json(
                        &serde_json::json!({"key": key, "value": v}),
                        args.pretty
                    )
                ),
                Err(e) => return Err(CliError::failed_with("[gait] error", e)),
            }
        }
        "show" => match gait_ops::config::resolve(&args.gait_dir, &args.gait_dir) {
            Ok(value) => println!("{}", gait_ops::render_json(&value, args.pretty)),
            Err(e) => return Err(CliError::failed_after("[gait]", e)),
        },
        _ => return Err(CliError::usage("[gait] usage: gait config <set|get|show> ...")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variant `cmd_*` returned, if it returned a `CliError`.
    fn cli_error(err: &anyhow::Error) -> Option<&CliError> {
        coded_error::downcast_ref::<CliError>(err)
    }

    fn inference_args(action: &str) -> InferenceArgs {
        InferenceArgs {
            gait_dir: tempfile::tempdir().unwrap().keep(),
            action: Some(action.to_string()),
            name: None,
            model: None,
            compaction_threshold_tokens: None,
            session: None,
            pretty: false,
        }
    }

    #[test]
    fn inference_show_without_a_name_is_usage() {
        let err = cmd_inference(inference_args("show")).expect_err("show needs a name");
        assert!(matches!(cli_error(&err), Some(CliError::Usage(_))), "{err:#}");
        assert_eq!(format!("{err:#}"), "[gait] usage: gait inference show <name>");
    }

    #[test]
    fn inference_unknown_action_is_usage() {
        let err = cmd_inference(inference_args("frobnicate")).expect_err("unknown action");
        assert!(matches!(cli_error(&err), Some(CliError::Usage(_))), "{err:#}");
    }

    #[test]
    fn inference_add_without_compaction_threshold_is_usage() {
        let mut args = inference_args("add");
        args.name = Some("fast".to_string());
        args.model = Some("opencode/kimi-k2.6".to_string());
        let err = cmd_inference(args).expect_err("add needs a threshold");
        assert!(matches!(cli_error(&err), Some(CliError::Usage(_))), "{err:#}");
    }

    #[test]
    fn inference_select_for_an_inactive_session_is_not_found() {
        let mut args = inference_args("select");
        args.name = Some("fast".to_string());
        args.session = Some("no-such-session".to_string());
        let err = cmd_inference(args).expect_err("session is not active");
        assert!(matches!(cli_error(&err), Some(CliError::NotFound(_))), "{err:#}");
        assert_eq!(
            format!("{err:#}"),
            "[gait] session 'no-such-session' is not active"
        );
    }

    #[test]
    fn mode_show_without_a_name_is_usage() {
        let args = ModeArgs {
            gait_dir: tempfile::tempdir().unwrap().keep(),
            action: Some("show".to_string()),
            name: None,
            allow: vec![],
            deny: vec![],
            steer: String::new(),
            description: String::new(),
            session: None,
            pretty: false,
        };
        let err = cmd_mode(args).expect_err("show needs a name");
        assert!(matches!(cli_error(&err), Some(CliError::Usage(_))), "{err:#}");
    }

    #[test]
    fn mode_clear_without_a_session_is_usage() {
        let args = ModeArgs {
            gait_dir: tempfile::tempdir().unwrap().keep(),
            action: Some("clear".to_string()),
            name: None,
            allow: vec![],
            deny: vec![],
            steer: String::new(),
            description: String::new(),
            session: None,
            pretty: false,
        };
        let err = cmd_mode(args).expect_err("clear needs a session");
        assert!(matches!(cli_error(&err), Some(CliError::Usage(_))), "{err:#}");
        assert_eq!(format!("{err:#}"), "[gait] --session is required");
    }

    #[test]
    fn config_set_without_a_key_is_usage() {
        let args = ConfigArgs {
            gait_dir: tempfile::tempdir().unwrap().keep(),
            action: Some("set".to_string()),
            rest: vec![],
            pretty: false,
        };
        let err = cmd_config(args).expect_err("set needs a key and value");
        assert!(matches!(cli_error(&err), Some(CliError::Usage(_))), "{err:#}");
        assert_eq!(
            format!("{err:#}"),
            "[gait] usage: gait config set <key> <value>"
        );
    }

    #[test]
    fn config_set_without_a_value_is_usage() {
        let args = ConfigArgs {
            gait_dir: tempfile::tempdir().unwrap().keep(),
            action: Some("set".to_string()),
            rest: vec!["default_model".to_string()],
            pretty: false,
        };
        let err = cmd_config(args).expect_err("set needs a value");
        assert!(matches!(cli_error(&err), Some(CliError::Usage(_))), "{err:#}");
    }

    #[test]
    fn config_unknown_action_is_usage() {
        let args = ConfigArgs {
            gait_dir: tempfile::tempdir().unwrap().keep(),
            action: Some("frobnicate".to_string()),
            rest: vec![],
            pretty: false,
        };
        let err = cmd_config(args).expect_err("unknown action");
        assert!(matches!(cli_error(&err), Some(CliError::Usage(_))), "{err:#}");
    }

    #[cfg(feature = "local")]
    #[test]
    fn local_unknown_action_is_usage() {
        let args = LocalArgs {
            gait_dir: tempfile::tempdir().unwrap().keep(),
            action: Some("frobnicate".to_string()),
            name: "local-qwen".to_string(),
            model_path: None,
            context_tokens: 8192,
            gpu_layers: 0,
            compaction_threshold_tokens: 6000,
            pretty: false,
        };
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(cmd_local(args))
            .expect_err("unknown local action");
        assert!(matches!(cli_error(&err), Some(CliError::Usage(_))), "{err:#}");
    }
}
