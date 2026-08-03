//! Registry/config administration + auth + models.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

use clap::Parser;
use std::path::PathBuf;

/// `gait auth list|add|remove|login|status|logout|link|unlink [provider] [source]`
#[derive(Parser, Debug)]
pub struct AuthArgs {
    /// Action: list (default), add, remove, login, status, logout, link, unlink
    pub action: Option<String>,
    /// Provider name
    pub provider: Option<String>,
    /// External source name (used with link)
    pub source: Option<String>,
    /// Override the external CLI's home directory
    #[arg(long)]
    pub home: Option<PathBuf>,
}

/// `gait models [filter]`
#[derive(Parser, Debug)]
pub struct ModelsArgs {
    /// Optional substring filter for model ids
    pub filter: Option<String>,
}

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
    #[arg(long)]
    pub session: Option<String>,
}

pub fn cmd_inference(args: InferenceArgs) {
    match args.action.as_deref().unwrap_or("list") {
        "list" => {
            for name in gait_ops::inference::list(&args.gait_dir) {
                println!("{name}");
            }
        }
        "show" => {
            let name = args.name.as_deref().unwrap_or_else(|| {
                eprintln!("[gait] usage: gait inference show <name>");
                std::process::exit(2)
            });
            match gait_ops::inference::show(&args.gait_dir, name) {
                Ok(i) => println!("{}\t{}", i.name, i.model),
                Err(e) => {
                    eprintln!("[gait] {e}");
                    std::process::exit(1)
                }
            }
        }
        "select" => {
            let name = args.name.as_deref().unwrap_or_else(|| {
                eprintln!("[gait] usage: gait inference select <name> --session <id>");
                std::process::exit(2)
            });
            let session = args.session.as_deref().unwrap_or_else(|| {
                eprintln!("[gait] --session is required");
                std::process::exit(2)
            });
            if let Err(e) = gait_ops::inference::select(&args.gait_dir, session, name) {
                eprintln!("[gait] {e}");
                std::process::exit(1);
            }
        }
        "clear" => {
            let session = args.session.as_deref().unwrap_or_else(|| {
                eprintln!("[gait] --session is required");
                std::process::exit(2)
            });
            if let Err(e) = gait_ops::inference::clear(&args.gait_dir, session) {
                eprintln!("[gait] {e}");
                std::process::exit(1);
            }
        }
        "add" => {
            let name = args.name.as_deref().unwrap_or_else(|| {
                eprintln!("[gait] usage: gait inference add <name> --model <provider/id>");
                std::process::exit(2)
            });
            let model = args.model.as_deref().unwrap_or_else(|| {
                eprintln!("[gait] usage: gait inference add <name> --model <provider/id>");
                std::process::exit(2)
            });
            match gait_ops::inference::add(&args.gait_dir, name, model) {
                Ok(i) => println!("added inference '{}' → model {}", i.name, i.model),
                Err(e) => {
                    eprintln!("[gait] {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("[gait] inference action must be list, show, select, clear, or add");
            std::process::exit(2)
        }
    }
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

/// `gait auth <list|add|remove> [<provider>]`. Thin adapter over
/// `gait_ops::auth`. The `add` branch keeps its own stdin read of the API
/// key (per design §4: fixed-value prompts → value passed as a param, but
/// the prompt itself stays in the adapter); it then delegates the actual
/// store write to the op.
pub async fn cmd_auth(args: AuthArgs) {
    // Normalize: action defaults to "list"; accept "ls" as alias.
    let action = args
        .action
        .as_deref()
        .map(|s| if s == "ls" { "list" } else { s })
        .unwrap_or("list");
    let provider = args.provider.as_deref();
    match action {
        "list" => {
            let creds = match gait_ops::auth::list() {
                Ok(credentials) => credentials,
                Err(error) => {
                    eprintln!("[gait] error: {error}");
                    std::process::exit(1);
                }
            };
            if creds.is_empty() {
                println!("No credentials configured. Run: gait auth add <provider>");
            } else {
                for (k, v) in &creds {
                    println!("  {}: {}", k, v);
                }
            }
        }
        "add" => {
            let provider = match provider {
                Some(p) => p,
                None => {
                    eprintln!("usage: gait auth add <provider>");
                    std::process::exit(1);
                }
            };
            if provider == "openai-codex" {
                eprintln!(
                    "openai-codex uses a ChatGPT subscription; run: gait auth login openai-codex"
                );
                std::process::exit(1);
            }
            eprint!("{} API key: ", provider);
            use std::io::BufRead;
            let key = std::io::BufReader::new(std::io::stdin())
                .lines()
                .next()
                .and_then(|l| l.ok())
                .unwrap_or_default()
                .trim()
                .to_string();
            match gait_ops::auth::add(provider, key) {
                Ok(out) => println!("Saved {} credentials ({})", out.provider, out.display),
                Err(e) => {
                    eprintln!("[gait] error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "login" => {
            let provider = provider.unwrap_or("openai-codex");
            if provider != "openai-codex" {
                eprintln!("subscription login is currently available only for openai-codex");
                std::process::exit(1);
            }
            match gait_ops::auth::login_codex(|flow| match flow {
                gait_auth::LoginFlow::DeviceCode {
                    verification_url,
                    user_code,
                    ..
                } => {
                    println!("Open {verification_url} and enter code: {user_code}");
                    println!("Waiting for approval...");
                }
                _ => unreachable!("Codex subscription login uses a device code"),
            })
            .await
            {
                Ok(out) => println!("Saved {} credentials ({})", out.provider, out.display),
                Err(error) => {
                    eprintln!("[gait] error: {error}");
                    std::process::exit(1);
                }
            }
        }
        "status" => {
            let provider = match provider {
                Some(provider) => provider,
                None => {
                    eprintln!("usage: gait auth status <provider>");
                    std::process::exit(1);
                }
            };
            match gait_ops::auth::status(provider) {
                Ok(Some(status)) => println!("{provider}: {status}"),
                Ok(None) => println!("{provider}: not configured"),
                Err(error) => {
                    eprintln!("[gait] error: {error}");
                    std::process::exit(1);
                }
            }
        }
        "link" => {
            let provider = match provider {
                Some(provider) => provider,
                None => {
                    eprintln!("usage: gait auth link <provider> <source> [--home <path>]");
                    std::process::exit(1);
                }
            };
            let source = match args.source.as_deref() {
                Some(source) => source,
                None => {
                    eprintln!("usage: gait auth link <provider> <source> [--home <path>]");
                    std::process::exit(1);
                }
            };
            match gait_ops::auth::link(provider, source, args.home) {
                Ok(out) => println!("Linked {} credentials ({})", out.provider, out.display),
                Err(error) => {
                    eprintln!("[gait] error: {error}");
                    std::process::exit(1);
                }
            }
        }
        "unlink" => {
            let provider = match provider {
                Some(provider) => provider,
                None => {
                    eprintln!("usage: gait auth unlink <provider>");
                    std::process::exit(1);
                }
            };
            match gait_ops::auth::unlink(provider) {
                Ok(out) => println!("Unlinked {} external credentials", out.provider),
                Err(error) => {
                    eprintln!("[gait] error: {error}");
                    std::process::exit(1);
                }
            }
        }
        "logout" => {
            let provider = provider.unwrap_or("openai-codex");
            if provider != "openai-codex" {
                eprintln!("subscription logout is currently available only for openai-codex; use `gait auth remove {provider}` for API keys");
                std::process::exit(1);
            }
            match gait_ops::auth::logout_codex().await {
                Ok(out) => println!("Logged out and removed {} credentials", out.provider),
                Err(error) => {
                    eprintln!("[gait] error: {error}");
                    std::process::exit(1);
                }
            }
        }
        "remove" | "rm" => {
            let provider = match provider {
                Some(p) => p,
                None => {
                    eprintln!("usage: gait auth remove <provider>");
                    std::process::exit(1);
                }
            };
            match gait_ops::auth::remove(provider) {
                Ok(out) => println!("Removed {} credentials", out.provider),
                Err(e) => {
                    eprintln!("[gait] error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        other => {
            eprintln!("unknown auth subcommand: {}", other);
            eprintln!("usage: gait auth <list|add|remove|login|status|logout|link|unlink> [<provider>] [<source>]");
            std::process::exit(1);
        }
    }
}

/// `gait models [filter]`. Thin adapter over `gait_ops::auth::models`.
/// The op owns the network read + filter; the adapter only renders.
pub async fn cmd_models(args: ModelsArgs) {
    let filter = args.filter.as_deref();
    match gait_ops::auth::models(filter).await {
        Ok(out) => {
            let filtered = out.render_filtered();
            for m in &filtered {
                println!("  {}/{}", m.provider, m.id);
            }
            println!(
                "\n{} models{}",
                filtered.len(),
                filter
                    .map(|f| format!(" matching \"{}\"", f))
                    .unwrap_or_default()
            );
        }
        Err(e) => {
            eprintln!("Error fetching models: {}", e);
            std::process::exit(1);
        }
    }
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

    let out = gait_ops::extension::register(params).map_err(|e| anyhow::anyhow!("{e}"))?;
    match out.scope {
        "agent" => println!(
            "Added {} '{}' to agent whitelist at {}.",
            args.kind,
            args.name,
            out.written_to.display()
        ),
        "session" => println!(
            "Added {} '{}' to session '{}' whitelist.",
            args.kind,
            args.name,
            args.session.as_deref().unwrap_or("")
        ),
        _ => {
            println!("Registered {} '{}'.", args.kind, args.name);
            if !out.hint.is_empty() {
                println!("→ {}", out.hint);
            }
        }
    }
    Ok(())
}

pub fn cmd_enable(args: EnableArgs) {
    let gait_dir = match crate::cli::args::resolve_gait_dir_from(args.gait_dir.clone()) {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("[gait] {error}");
            std::process::exit(1);
        }
    };
    let params = gait_ops::extension::EnableParams {
        gait_dir,
        kind: args.kind.clone(),
        name: args.name.clone(),
    };
    match gait_ops::extension::enable(params) {
        Ok(_) => eprintln!("Enabled {} '{}'.", args.kind, args.name),
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn cmd_disable(args: DisableArgs) {
    let gait_dir = match crate::cli::args::resolve_gait_dir_from(args.gait_dir.clone()) {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("[gait] {error}");
            std::process::exit(1);
        }
    };
    let params = gait_ops::extension::EnableParams {
        gait_dir,
        kind: args.kind.clone(),
        name: args.name.clone(),
    };
    match gait_ops::extension::disable(params) {
        Ok(_) => eprintln!("Disabled {} '{}'.", args.kind, args.name),
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    }
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

    let out = gait_ops::extension::unregister(params).map_err(|e| anyhow::anyhow!("{e}"))?;
    match out.scope {
        "agent" => println!(
            "Removed {} '{}' from agent whitelist at {}.",
            args.kind,
            args.name,
            out.written_to.display()
        ),
        "session" => println!(
            "Removed {} '{}' from session '{}' whitelist (still visible if the agent's own whitelist also grants it).",
            args.kind,
            args.name,
            args.session.as_deref().unwrap_or("")
        ),
        _ => eprintln!("Removed {} '{}'.", args.kind, args.name),
    }
    Ok(())
}

/// Parse a `gait config set` value: try JSON first (numbers, bools, arrays,
/// objects), fall back to a raw string (so `anthropic/claude-opus-4` doesn't
/// need quoting) — matches the existing ergonomic of the generic `set`.
fn parse_config_value(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

/// `gait config <set|get> ...` — the general external-CLI equivalent of any
/// REPL slash command that mutates `gait.jsonc`. Calls the generic
/// `gait_ops::config::set`/`get` directly (the comment-safe jsonc mutators).
/// gait-dir-wide, not session-scoped. Mirrors `WorkflowArgs`/`cmd_workflow`'s
/// action+rest shape exactly.
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct ConfigArgs {
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    /// Action: set | get
    pub action: Option<String>,
    /// Remaining args: `<key> <value>` for `set`, `<key>` for `get`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

pub fn cmd_config(args: ConfigArgs) {
    let sub = args.action.as_deref().unwrap_or("");
    match sub {
        "set" => {
            let key = args.rest.first().cloned().unwrap_or_else(|| {
                eprintln!("[gait] usage: gait config set <key> <value>");
                std::process::exit(1);
            });
            let value_raw = args.rest.get(1).cloned().unwrap_or_else(|| {
                eprintln!("[gait] usage: gait config set <key> <value>");
                std::process::exit(1);
            });
            let value = parse_config_value(&value_raw);
            match gait_ops::config::set(&args.gait_dir, &key, value) {
                Ok(()) => eprintln!("[gait] {} set to {}", key, value_raw),
                Err(e) => {
                    eprintln!("[gait] error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "get" => {
            let key = args.rest.first().cloned().unwrap_or_else(|| {
                eprintln!("[gait] usage: gait config get <key>");
                std::process::exit(1);
            });
            match gait_ops::config::get(&args.gait_dir, &key) {
                Ok(Some(v)) => println!("{}", v),
                Ok(None) => println!("(unset)"),
                Err(e) => {
                    eprintln!("[gait] error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("[gait] usage: gait config <set|get> ...");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_link_parses_source_and_home() {
        let args = AuthArgs::try_parse_from([
            "gait-auth",
            "link",
            "openai-codex",
            "codex",
            "--home",
            "/tmp/codex-home",
        ])
        .unwrap();
        assert_eq!(args.action.as_deref(), Some("link"));
        assert_eq!(args.provider.as_deref(), Some("openai-codex"));
        assert_eq!(args.source.as_deref(), Some("codex"));
        assert_eq!(args.home, Some(PathBuf::from("/tmp/codex-home")));
    }
}
