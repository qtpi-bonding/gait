//! Registry/config administration + auth + models.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

use clap::Parser;
use std::path::PathBuf;

/// `gait auth list|add|remove [provider]`
#[derive(Parser, Debug)]
pub struct AuthArgs {
    /// Action: list (default), add, remove
    pub action: Option<String>,
    /// Provider name (used with add/remove)
    pub provider: Option<String>,
}

/// `gait models [filter]`
#[derive(Parser, Debug)]
pub struct ModelsArgs {
    /// Optional substring filter for model ids
    pub filter: Option<String>,
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
pub fn cmd_auth(args: AuthArgs) {
    // Normalize: action defaults to "list"; accept "ls" as alias.
    let action = args
        .action
        .as_deref()
        .map(|s| if s == "ls" { "list" } else { s })
        .unwrap_or("list");
    let provider = args.provider.as_deref();
    match action {
        "list" => {
            let creds = gait_ops::auth::list();
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
            eprintln!("usage: gait auth <list|add|remove> [<provider>]");
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
    let gait_dir: PathBuf = args
        .gait_dir
        .clone()
        .or_else(|| {
            std::env::var("GAIT_DIR")
                .ok()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .ok_or_else(|| anyhow::anyhow!("no gait dir: pass --gait-dir <path> or set $GAIT_DIR"))?;

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
    let gait_dir: PathBuf = match args.gait_dir.clone().or_else(|| {
        std::env::var("GAIT_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }) {
        Some(d) => d,
        None => {
            eprintln!("[gait] no gait dir: pass --gait-dir <path> or set $GAIT_DIR");
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
    let gait_dir: PathBuf = match args.gait_dir.clone().or_else(|| {
        std::env::var("GAIT_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }) {
        Some(d) => d,
        None => {
            eprintln!("[gait] no gait dir: pass --gait-dir <path> or set $GAIT_DIR");
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
    let gait_dir: PathBuf = args
        .gait_dir
        .clone()
        .or_else(|| {
            std::env::var("GAIT_DIR")
                .ok()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .ok_or_else(|| anyhow::anyhow!("no gait dir: pass --gait-dir <path> or set $GAIT_DIR"))?;

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
