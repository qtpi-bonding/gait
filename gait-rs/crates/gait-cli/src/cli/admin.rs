//! Registry/config administration + auth + models.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

use clap::Parser;
use serde_json::{json, Value};
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

pub fn read_config_as_value(config_path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let raw = std::fs::read_to_string(config_path).unwrap_or_else(|_| "{}".into());
    let stripped = gait_core::config::strip_jsonc_comments(&raw);
    Ok(serde_json::from_str(&stripped).unwrap_or_default())
}

/// Update a boolean key in YAML frontmatter of a markdown file, preserving the body.
pub fn set_frontmatter_bool(content: &str, key: &str, value: bool) -> anyhow::Result<String> {
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

/// Append a value to a YAML list at `key` in frontmatter, creating the key if absent
/// and creating frontmatter entirely if the file has none. Avoids duplicate entries.
pub fn append_frontmatter_list(content: &str, key: &str, value: &str) -> anyhow::Result<String> {
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

/// Remove a value from the YAML list at `key` in frontmatter. No-op if key or value is absent.
pub fn remove_frontmatter_list(content: &str, key: &str, value: &str) -> anyhow::Result<String> {
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
        if let Some(entry) = map.get_mut(&serde_yaml::Value::String(key.to_string())) {
            if let serde_yaml::Value::Sequence(ref mut seq) = entry {
                seq.retain(|v| v.as_str().map(|s| s != value).unwrap_or(true));
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

pub fn cmd_auth(args: AuthArgs) {
    use gait_core::auth;
    // Normalize: action defaults to "list"; accept "ls" as alias.
    let action = args
        .action
        .as_deref()
        .map(|s| if s == "ls" { "list" } else { s })
        .unwrap_or("list");
    let provider = args.provider.as_deref();
    match action {
        "list" => {
            let creds = auth::list_credentials();
            if creds.is_empty() {
                println!("No credentials configured. Run: gait auth add <provider>");
            } else {
                let mut sorted: Vec<_> = creds.into_iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                for (k, v) in sorted {
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
            if key.is_empty() {
                eprintln!("empty key, aborted");
                std::process::exit(1);
            }
            if let Err(e) = auth::write_api_key(provider, &key) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
            println!("Saved {} credentials", provider);
        }
        "remove" | "rm" => {
            let provider = match provider {
                Some(p) => p,
                None => {
                    eprintln!("usage: gait auth remove <provider>");
                    std::process::exit(1);
                }
            };
            if let Err(e) = auth::remove_api_key(provider) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
            println!("Removed {} credentials", provider);
        }
        other => {
            eprintln!("unknown auth subcommand: {}", other);
            eprintln!("usage: gait auth <list|add|remove> [<provider>]");
            std::process::exit(1);
        }
    }
}

pub async fn cmd_models(args: ModelsArgs) {
    use gait_core::models;
    let filter = args.filter.as_deref();
    match models::get_models().await {
        Ok(models) => {
            let filtered = models::filter_models(&models, filter);
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

fn build_flags_from_register_args(
    args: &RegisterArgs,
) -> std::collections::HashMap<String, String> {
    let mut f = std::collections::HashMap::new();
    if let Some(v) = &args.url {
        f.insert("url".into(), v.clone());
    }
    if let Some(v) = &args.command {
        f.insert("command".into(), v.clone());
    }
    if let Some(v) = &args.args {
        f.insert("args".into(), v.clone());
    }
    if let Some(v) = &args.spawn {
        f.insert("spawn".into(), v.clone());
    }
    if let Some(v) = &args.connect {
        f.insert("connect".into(), v.clone());
    }
    if let Some(v) = &args.description {
        f.insert("description".into(), v.clone());
    }
    if let Some(v) = &args.content {
        f.insert("content".into(), v.clone());
    }
    if let Some(v) = &args.path {
        f.insert("path".into(), v.clone());
    }
    if let Some(v) = &args.wasm {
        f.insert("wasm".into(), v.clone());
    }
    f
}

pub fn cmd_register(args: RegisterArgs) -> anyhow::Result<()> {
    // Supported usage:
    //   gait register mcp <name> --command <prog> [--args "<args>"]
    //   gait register mcp <name> --spawn <cmd>  (compat)
    //   gait register mcp <name> --url <url>
    //   gait register mcp <name> --connect <url>  (compat)
    //   gait register skill <name> --path <file>
    //   gait register skill <name> --description <text> --content <text>
    // Tier 1 (disk): MCPs → mcp/<name>.json
    // Tier 2 (legacy): Skills, old MCPs → gait.jsonc

    let kind = args.kind.as_str();
    let name = args.name.as_str();
    let flags = build_flags_from_register_args(&args);
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

    // Phase 1: --scope agent only touches the agent file's frontmatter list.
    // It never falls through to the global content-store branch below.
    if args.scope.as_deref() == Some("agent") {
        let agent_path = args
            .agent
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--scope agent requires --agent <path>"))?;
        let list_key = match kind {
            "skill" => "skills",
            "mcp" => "mcp",
            "plugin" => "plugins",
            other => {
                return Err(anyhow::anyhow!(
                    "unknown kind '{}': use mcp, skill, or plugin",
                    other
                ))
            }
        };
        let content = std::fs::read_to_string(&agent_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to read agent file '{}': {}",
                agent_path.display(),
                e
            )
        })?;
        let new_content = append_frontmatter_list(&content, list_key, name)?;
        std::fs::write(&agent_path, new_content)?;
        println!(
            "Added {} '{}' to agent whitelist at {}.",
            kind,
            name,
            agent_path.display()
        );
        return Ok(());
    }

    match kind {
        "mcp" => {
            let mcp_dir = gait_dir.join("mcp");
            std::fs::create_dir_all(&mcp_dir)
                .map_err(|e| anyhow::anyhow!("could not create mcp/ dir: {e}"))?;
            let disk_path = mcp_dir.join(format!("{}.json", name));
            if disk_path.exists() {
                return Err(anyhow::anyhow!(
                    "mcp '{}' already registered at {}. Use 'gait unregister mcp {}' first.",
                    name,
                    disk_path.display(),
                    name
                ));
            }
            let entry = if let Some(cmd) = flags.get("command") {
                let cmd_args: Vec<serde_json::Value> = flags
                    .get("args")
                    .map(|s| {
                        s.split_whitespace()
                            .map(|a| serde_json::Value::String(a.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                json!({ "command": cmd, "args": cmd_args, "enabled": false })
            } else if let Some(spawn) = flags.get("spawn") {
                let mut parts = spawn.split_whitespace();
                let prog = parts.next().unwrap_or("").to_string();
                let rest: Vec<serde_json::Value> = parts
                    .map(|s| serde_json::Value::String(s.to_string()))
                    .collect();
                json!({ "command": prog, "args": rest, "enabled": false })
            } else if let Some(url) = flags.get("url").or_else(|| flags.get("connect")) {
                json!({ "url": url, "enabled": false })
            } else {
                return Err(anyhow::anyhow!(
                    "gait register mcp: requires --command <prog> [--args \"<args>\"], --spawn <cmd>, or --url <url>"
                ));
            };
            if let Some(desc) = flags.get("description") {
                let mut obj = entry.as_object().unwrap().clone();
                obj.insert(
                    "description".into(),
                    serde_json::Value::String(desc.clone()),
                );
                std::fs::write(
                    &disk_path,
                    serde_json::to_string_pretty(&Value::Object(obj))?,
                )?;
            } else {
                std::fs::write(&disk_path, serde_json::to_string_pretty(&entry)?)?;
            }
            println!("Registered mcp '{}'.", name);
            println!("→ Run 'gait enable mcp {}' to activate it.", name);
            Ok(())
        }
        "skill" => {
            let skills_root = gait_dir.join("skills");
            std::fs::create_dir_all(&skills_root)
                .map_err(|e| anyhow::anyhow!("could not create skills/ dir: {e}"))?;
            let skill_dir = skills_root.join(name);
            if skill_dir.exists() {
                return Err(anyhow::anyhow!(
                    "skill '{}' already registered at {}. Use 'gait unregister skill {}' first.",
                    name,
                    skill_dir.display(),
                    name
                ));
            }
            let description = flags.get("description").ok_or_else(|| {
                anyhow::anyhow!("gait register skill: requires --description <text>")
            })?;
            let content = if let Some(path) = flags.get("path") {
                std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("failed to read skill file '{}': {}", path, e))?
            } else if let Some(content) = flags.get("content") {
                content.clone()
            } else {
                return Err(anyhow::anyhow!(
                    "gait register skill: requires --path <file> or --content <text>"
                ));
            };
            std::fs::create_dir_all(&skill_dir)
                .map_err(|e| anyhow::anyhow!("could not create skills/{} dir: {e}", name))?;
            let skill_md = skill_dir.join("SKILL.md");
            std::fs::write(
                &skill_md,
                format!(
                    "---\nname: {}\ndescription: {}\nenabled: false\n---\n\n{}",
                    name, description, content
                ),
            )?;
            println!("Registered skill '{}'.", name);
            println!("→ Run 'gait enable skill {}' to activate it.", name);
            Ok(())
        }
        "plugin" => {
            let wasm_src = flags
                .get("wasm")
                .ok_or_else(|| anyhow::anyhow!("gait register plugin: requires --wasm <path>"))?;
            let wasm_src_path = std::path::Path::new(wasm_src);
            if !wasm_src_path.exists() {
                return Err(anyhow::anyhow!("wasm file not found: {}", wasm_src));
            }
            let plugin_dir = gait_dir.join("plugins").join(name);
            if plugin_dir.exists() {
                return Err(anyhow::anyhow!(
                    "plugin '{}' already registered at {}. Use 'gait unregister plugin {}' first.",
                    name,
                    plugin_dir.display(),
                    name
                ));
            }
            std::fs::create_dir_all(&plugin_dir)
                .map_err(|e| anyhow::anyhow!("could not create plugins/{} dir: {e}", name))?;
            let wasm_filename = wasm_src_path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("invalid wasm path: {}", wasm_src))?;
            let wasm_dest = plugin_dir.join(wasm_filename);
            std::fs::copy(wasm_src_path, &wasm_dest)
                .map_err(|e| anyhow::anyhow!("could not copy wasm file: {e}"))?;
            let wasm_rel = wasm_filename.to_string_lossy();
            let description = flags
                .get("description")
                .map(|s| s.as_str())
                .unwrap_or("No description.");
            let plugin_md = plugin_dir.join("PLUGIN.md");
            std::fs::write(
                &plugin_md,
                format!(
                    "---\nname: {}\nwasm: {}\nenabled: false\nallow_exec: []\nallow_fs: []\n---\n\n# {}\n\n{}\n",
                    name, wasm_rel, name, description
                ),
            )?;
            println!("Registered plugin '{}'.", name);
            println!("→ Run 'gait enable plugin {}' to activate it.", name);
            Ok(())
        }
        other => Err(anyhow::anyhow!(
            "unknown kind '{}': use mcp, skill, or plugin",
            other
        )),
    }
}

pub(crate) fn cmd_enable_disable_inner(
    gait_dir: &std::path::Path,
    kind: &str,
    name: &str,
    enable: bool,
) -> anyhow::Result<()> {
    // Disk-file path for mcp, plugin, and skill kinds
    let disk_path: Option<std::path::PathBuf> = match kind {
        "mcp" => {
            let p = gait_dir.join("mcp").join(format!("{}.json", name));
            if p.exists() {
                Some(p)
            } else {
                None
            }
        }
        "plugin" => {
            let p = gait_dir.join("plugins").join(name).join("PLUGIN.md");
            if p.exists() {
                Some(p)
            } else {
                None
            }
        }
        "skill" => {
            let p = gait_dir.join("skills").join(name).join("SKILL.md");
            if p.exists() {
                Some(p)
            } else {
                None
            }
        }
        _ => None,
    };

    if let Some(path) = disk_path {
        match kind {
            "mcp" => {
                let mut val: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(&path)?)?;
                val["enabled"] = serde_json::Value::Bool(enable);
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            }
            "plugin" => {
                let content = std::fs::read_to_string(&path)?;
                let new_content = set_frontmatter_bool(&content, "enabled", enable)?;
                std::fs::write(&path, new_content)?;
            }
            "skill" => {
                let content = std::fs::read_to_string(&path)?;
                let new_content = set_frontmatter_bool(&content, "enabled", enable)?;
                std::fs::write(&path, new_content)?;
            }
            _ => unreachable!(),
        }
        eprintln!(
            "{} {} '{}'.",
            if enable { "Enabled" } else { "Disabled" },
            kind,
            name
        );
        return Ok(());
    }

    // Fall back to gait.jsonc for legacy mcp/skill entries
    let array_key = match kind {
        "mcp" => "mcp_servers",
        "skill" => "skills",
        other => {
            return Err(anyhow::anyhow!(
                "unknown kind '{}': use mcp, skill, or plugin",
                other
            ))
        }
    };
    let config_path = gait_dir.join("gait.jsonc");
    let mut val = read_config_as_value(&config_path)?;
    let arr = val[array_key]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not an array", array_key))?;
    let entry = arr
        .iter_mut()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some(name))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} '{}' not found; run 'gait register {} {}' first",
                kind,
                name,
                kind,
                name
            )
        })?;
    entry["enabled"] = serde_json::Value::Bool(enable);
    eprintln!("Note: gait.jsonc comments are stripped on rewrite.");
    std::fs::write(&config_path, serde_json::to_string_pretty(&val)?)?;
    eprintln!(
        "{} {} '{}'.",
        if enable { "Enabled" } else { "Disabled" },
        kind,
        name
    );
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
    if let Err(e) = cmd_enable_disable_inner(&gait_dir, &args.kind, &args.name, true) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
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
    if let Err(e) = cmd_enable_disable_inner(&gait_dir, &args.kind, &args.name, false) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
}

pub fn cmd_unregister(args: UnregisterArgs) -> anyhow::Result<()> {
    let kind = args.kind.as_str();
    let name = args.name.as_str();
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

    // Phase 1: --scope agent only touches the agent file's frontmatter list.
    // It never falls through to the global disk-entry branch below.
    if args.scope.as_deref() == Some("agent") {
        let agent_path = args
            .agent
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--scope agent requires --agent <path>"))?;
        let list_key = match kind {
            "skill" => "skills",
            "mcp" => "mcp",
            "plugin" => "plugins",
            other => {
                return Err(anyhow::anyhow!(
                    "unknown kind '{}': use mcp, skill, or plugin",
                    other
                ))
            }
        };
        let content = std::fs::read_to_string(&agent_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to read agent file '{}': {}",
                agent_path.display(),
                e
            )
        })?;
        let new_content = remove_frontmatter_list(&content, list_key, name)?;
        std::fs::write(&agent_path, new_content)?;
        println!(
            "Removed {} '{}' from agent whitelist at {}.",
            kind,
            name,
            agent_path.display()
        );
        return Ok(());
    }

    // Disk-first for mcp, plugin, and skill
    if kind == "mcp" {
        let disk_path = gait_dir.join("mcp").join(format!("{}.json", name));
        if disk_path.exists() {
            std::fs::remove_file(&disk_path)?;
            eprintln!("Removed mcp '{}'.", name);
            return Ok(());
        }
    }
    if kind == "plugin" {
        let plugin_dir = gait_dir.join("plugins").join(name);
        if plugin_dir.exists() {
            std::fs::remove_dir_all(&plugin_dir)?;
            eprintln!("Removed plugin '{}'.", name);
            return Ok(());
        }
        return Err(anyhow::anyhow!("plugin '{}' not found", name));
    }
    if kind == "skill" {
        let skill_dir = gait_dir.join("skills").join(name);
        if skill_dir.exists() {
            std::fs::remove_dir_all(&skill_dir)?;
            eprintln!("Removed skill '{}'.", name);
            return Ok(());
        }
        return Err(anyhow::anyhow!("skill '{}' not found", name));
    }

    // Fall back to gait.jsonc
    let array_key = match kind {
        "mcp" => "mcp_servers",
        "skill" => "skills",
        other => {
            return Err(anyhow::anyhow!(
                "unknown kind '{}': use mcp, skill, or plugin",
                other
            ))
        }
    };
    let config_path = gait_dir.join("gait.jsonc");
    let mut val = read_config_as_value(&config_path)?;
    if let Some(arr) = val[array_key].as_array_mut() {
        arr.retain(|v| v.get("name").and_then(|n| n.as_str()) != Some(name));
    }
    eprintln!("Note: gait.jsonc comments are stripped on rewrite.");
    std::fs::write(&config_path, serde_json::to_string_pretty(&val)?)?;
    eprintln!("Removed {} '{}'.", kind, name);
    Ok(())
}

#[cfg(test)]
mod tests {
    fn reg(kind: &str, name: &str, dir: &std::path::Path) -> RegisterArgs {
        RegisterArgs {
            kind: kind.into(),
            name: name.into(),
            url: None,
            command: None,
            args: None,
            spawn: None,
            connect: None,
            description: None,
            content: None,
            path: None,
            wasm: None,
            env: vec![],
            yes: false,
            gait_dir: Some(dir.to_path_buf()),
            scope: None,
            agent: None,
        }
    }
    fn reg_flags(kind: &str, name: &str, dir: &std::path::Path, flags: &[&str]) -> RegisterArgs {
        let mut a = reg(kind, name, dir);
        for chunk in flags.chunks(2) {
            if chunk.len() != 2 {
                continue;
            }
            let v = chunk[1].to_string();
            match chunk[0] {
                "--spawn" => a.spawn = Some(v),
                "--args" => a.args = Some(v),
                "--description" => a.description = Some(v),
                "--content" => a.content = Some(v),
                "--path" => a.path = Some(v),
                "--wasm" => a.wasm = Some(v),
                "--command" => a.command = Some(v),
                _ => {}
            }
        }
        a
    }
    fn unreg(kind: &str, name: &str, dir: &std::path::Path) -> UnregisterArgs {
        UnregisterArgs {
            kind: kind.into(),
            name: name.into(),
            gait_dir: Some(dir.to_path_buf()),
            scope: None,
            agent: None,
        }
    }
    use super::*;

    #[test]
    fn cmd_register_mcp_writes_entry_disabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"anthropic/claude-sonnet-4-6","mcp_servers":[]}"#,
        )
        .unwrap();
        let _args: Vec<String> = ["mcp", "slack", "--spawn", "node mcp-slack.js"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        cmd_register(reg_flags(
            "mcp",
            "slack",
            dir.path(),
            &["--spawn", "node mcp-slack.js"],
        ))
        .unwrap();
        let val: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("mcp").join("slack.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(val["command"], "node");
        assert_eq!(val["args"][0], "mcp-slack.js");
        assert_eq!(val["enabled"], false, "load should default to disabled");
    }

    #[test]
    fn cmd_register_skill_writes_entry() {
        let dir = tempfile::tempdir().unwrap();
        // Skill registration writes to disk directly — no gait.jsonc needed.
        cmd_register(reg_flags(
            "skill",
            "tdd",
            dir.path(),
            &["--description", "tdd workflow", "--content", "# TDD"],
        ))
        .unwrap();
        let skill_md = dir.path().join("skills").join("tdd").join("SKILL.md");
        assert!(skill_md.exists(), "skills/tdd/SKILL.md should be created");
        let md = std::fs::read_to_string(&skill_md).unwrap();
        assert!(md.contains("name: tdd"));
        assert!(md.contains("description: tdd workflow"));
        assert!(md.contains("enabled: false"));
        assert!(md.contains("# TDD"));
    }

    #[test]
    fn cmd_enable_skill_prefers_disk_file() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("skills").join("tdd");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: tdd\ndescription: x\nenabled: false\n---\n# body\n",
        )
        .unwrap();
        cmd_enable_disable_inner(dir.path(), "skill", "tdd", true).unwrap();
        let md = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(md.contains("enabled: true"), "got: {md}");
        assert!(
            !md.contains("enabled: false"),
            "old value should be gone: {md}"
        );
    }

    #[test]
    fn cmd_unregister_skill_removes_directory() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("skills").join("tdd");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: tdd\ndescription: x\nenabled: false\n---\n",
        )
        .unwrap();
        cmd_unregister(unreg("skill", "tdd", dir.path())).unwrap();
        assert!(!skill_dir.exists(), "skills/tdd/ should be deleted");
    }

    // --- Phase 1: --scope agent tests (frontmatter-list whitelisting) ---

    fn write_agent(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let p = dir.join("agent.md");
        std::fs::write(&p, body).unwrap();
        p
    }

    fn reg_with_scope(
        kind: &str,
        name: &str,
        dir: &std::path::Path,
        agent: std::path::PathBuf,
    ) -> RegisterArgs {
        let mut a = reg(kind, name, dir);
        a.scope = Some("agent".into());
        a.agent = Some(agent);
        a
    }

    fn unreg_with_scope(
        kind: &str,
        name: &str,
        dir: &std::path::Path,
        agent: std::path::PathBuf,
    ) -> UnregisterArgs {
        let mut a = unreg(kind, name, dir);
        a.scope = Some("agent".into());
        a.agent = Some(agent);
        a
    }

    #[test]
    fn cmd_register_scope_agent_appends_to_agent_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let agent_path = write_agent(
            dir.path(),
            "---\nname: myagent\nallow:\n  - bash\n---\n# body\n",
        );
        cmd_register(reg_with_scope(
            "skill",
            "tdd",
            dir.path(),
            agent_path.clone(),
        ))
        .unwrap();
        let md = std::fs::read_to_string(&agent_path).unwrap();
        // Skill name is appended to the frontmatter list
        assert!(
            md.contains("- tdd"),
            "skills list should include tdd, got: {md}"
        );
        // Original entries preserved
        assert!(
            md.contains("allow:"),
            "existing allow key preserved, got: {md}"
        );
        assert!(
            md.contains("- bash"),
            "existing allow entry preserved, got: {md}"
        );
        // Body preserved
        assert!(md.contains("# body"), "body preserved, got: {md}");
        // The global skills/ dir was NOT created — agent scope never touches it
        assert!(
            !dir.path().join("skills").exists(),
            "agent scope must not write to global skills/"
        );
    }

    #[test]
    fn cmd_unregister_scope_agent_removes_from_agent_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let agent_path = write_agent(
            dir.path(),
            "---\nname: myagent\nskills:\n  - tdd\n---\n# body\n",
        );
        cmd_unregister(unreg_with_scope(
            "skill",
            "tdd",
            dir.path(),
            agent_path.clone(),
        ))
        .unwrap();
        let md = std::fs::read_to_string(&agent_path).unwrap();
        // The skill name is removed from the list
        assert!(!md.contains("- tdd"), "tdd should be removed, got: {md}");
        // The key may be empty (sequence) or absent; either way, no - tdd entry
        // Body preserved
        assert!(md.contains("# body"), "body preserved, got: {md}");
        // The global skills/ dir was NOT created
        assert!(
            !dir.path().join("skills").exists(),
            "agent scope must not write to global skills/"
        );
    }

    #[test]
    fn cmd_register_scope_agent_preserves_existing_list_entries() {
        let dir = tempfile::tempdir().unwrap();
        let agent_path = write_agent(
            dir.path(),
            "---\nname: myagent\nskills:\n  - existing-a\n  - existing-b\n---\n# body\n",
        );
        cmd_register(reg_with_scope(
            "skill",
            "tdd",
            dir.path(),
            agent_path.clone(),
        ))
        .unwrap();
        let md = std::fs::read_to_string(&agent_path).unwrap();
        // New entry appended
        assert!(md.contains("- tdd"), "tdd appended, got: {md}");
        // Existing entries preserved
        assert!(
            md.contains("- existing-a"),
            "existing-a preserved, got: {md}"
        );
        assert!(
            md.contains("- existing-b"),
            "existing-b preserved, got: {md}"
        );
    }

    #[test]
    fn cmd_unregister_scope_agent_preserves_other_list_entries() {
        let dir = tempfile::tempdir().unwrap();
        let agent_path = write_agent(
            dir.path(),
            "---\nname: myagent\nskills:\n  - tdd\n  - keep-me-a\n  - keep-me-b\n---\n# body\n",
        );
        cmd_unregister(unreg_with_scope(
            "skill",
            "tdd",
            dir.path(),
            agent_path.clone(),
        ))
        .unwrap();
        let md = std::fs::read_to_string(&agent_path).unwrap();
        // Target removed
        assert!(!md.contains("- tdd"), "tdd removed, got: {md}");
        // Other entries preserved
        assert!(md.contains("- keep-me-a"), "keep-me-a preserved, got: {md}");
        assert!(md.contains("- keep-me-b"), "keep-me-b preserved, got: {md}");
    }

    #[test]
    fn cmd_register_scope_agent_requires_agent_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = reg("skill", "tdd", dir.path());
        a.scope = Some("agent".into());
        // No --agent
        let err = cmd_register(a).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--agent"),
            "error should mention --agent, got: {msg}"
        );
    }

    #[test]
    fn cmd_unregister_scope_agent_requires_agent_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = unreg("skill", "tdd", dir.path());
        a.scope = Some("agent".into());
        // No --agent
        let err = cmd_unregister(a).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("--agent"),
            "error should mention --agent, got: {msg}"
        );
    }

    #[test]
    fn append_frontmatter_list_creates_list_when_absent() {
        let content = "---\nname: foo\n---\n# body\n";
        let result = append_frontmatter_list(content, "skills", "tdd").unwrap();
        assert!(
            result.contains("skills:"),
            "skills key should exist: {result}"
        );
        assert!(result.contains("- tdd"), "tdd entry should exist: {result}");
        assert!(
            result.contains("name: foo"),
            "existing key preserved: {result}"
        );
        assert!(result.contains("# body"), "body preserved: {result}");
    }

    #[test]
    fn append_frontmatter_list_avoids_duplicates() {
        let content = "---\nname: foo\nskills:\n  - tdd\n---\n# body\n";
        let result = append_frontmatter_list(content, "skills", "tdd").unwrap();
        // Count occurrences of "- tdd"
        let count = result.matches("- tdd").count();
        assert_eq!(count, 1, "tdd should appear only once, got: {result}");
    }

    #[test]
    fn remove_frontmatter_list_is_noop_when_absent() {
        let content = "---\nname: foo\n---\n# body\n";
        let result = remove_frontmatter_list(content, "skills", "tdd").unwrap();
        assert!(result.contains("name: foo"), "preserved: {result}");
        assert!(result.contains("# body"), "body preserved: {result}");
    }

    #[test]
    fn remove_frontmatter_list_removes_target_only() {
        let content = "---\nname: foo\nskills:\n  - tdd\n  - other\n---\n# body\n";
        let result = remove_frontmatter_list(content, "skills", "tdd").unwrap();
        assert!(!result.contains("- tdd"), "tdd removed: {result}");
        assert!(result.contains("- other"), "other preserved: {result}");
    }

    #[test]
    fn cmd_enable_sets_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[{"name":"slack","spawn":"node s.js","enabled":false}]}"#,
        )
        .unwrap();
        let _args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
        cmd_enable_disable_inner(dir.path(), "mcp", "slack", true).unwrap();
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap())
                .unwrap();
        assert_eq!(val["mcp_servers"][0]["enabled"], true);
    }

    #[test]
    fn cmd_disable_sets_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[{"name":"slack","spawn":"node s.js","enabled":true}]}"#,
        )
        .unwrap();
        let _args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
        cmd_enable_disable_inner(dir.path(), "mcp", "slack", false).unwrap();
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap())
                .unwrap();
        assert_eq!(val["mcp_servers"][0]["enabled"], false);
    }

    #[test]
    fn cmd_enable_missing_entry_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[]}"#,
        )
        .unwrap();
        let _args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
        assert!(cmd_enable_disable_inner(dir.path(), "mcp", "slack", true).is_err());
    }

    #[test]
    fn cmd_unregister_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[{"name":"slack","spawn":"node s.js","enabled":true}]}"#,
        )
        .unwrap();
        let _args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
        cmd_unregister(unreg("mcp", "slack", dir.path())).unwrap();
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap())
                .unwrap();
        assert!(val["mcp_servers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn cmd_register_plugin_missing_wasm_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"x"}"#).unwrap();
        let _args: Vec<String> = ["plugin", "foo"].iter().map(|s| s.to_string()).collect();
        assert!(cmd_register(reg("plugin", "foo", dir.path())).is_err());
    }

    #[test]
    fn cmd_register_mcp_writes_disk_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"anthropic/claude-sonnet-4-6"}"#,
        )
        .unwrap();
        let _args: Vec<String> = [
            "mcp",
            "slack",
            "--command",
            "npx",
            "--args",
            "-y @pkg/slack",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        cmd_register(reg_flags(
            "mcp",
            "slack",
            dir.path(),
            &["--command", "npx", "--args", "-y @pkg/slack"],
        ))
        .unwrap();
        let disk_path = dir.path().join("mcp").join("slack.json");
        assert!(disk_path.exists(), "mcp/slack.json should be created");
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&disk_path).unwrap()).unwrap();
        assert_eq!(val["command"], "npx");
        assert_eq!(val["enabled"], false);
    }

    #[test]
    fn cmd_register_mcp_spawn_converts_to_command_args() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"anthropic/claude-sonnet-4-6"}"#,
        )
        .unwrap();
        let _args: Vec<String> = ["mcp", "slack2", "--spawn", "node mcp-slack.js"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        cmd_register(reg_flags(
            "mcp",
            "slack2",
            dir.path(),
            &["--spawn", "node mcp-slack.js"],
        ))
        .unwrap();
        let val: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("mcp").join("slack2.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(val["command"], "node");
        assert_eq!(val["args"][0], "mcp-slack.js");
    }

    #[test]
    fn cmd_enable_mcp_prefers_disk_file() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_dir = dir.path().join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(
            mcp_dir.join("slack.json"),
            r#"{"command":"npx","args":[],"enabled":false}"#,
        )
        .unwrap();
        let _args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
        cmd_enable_disable_inner(dir.path(), "mcp", "slack", true).unwrap();
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(mcp_dir.join("slack.json")).unwrap())
                .unwrap();
        assert_eq!(val["enabled"], true);
    }

    #[test]
    fn cmd_unregister_mcp_removes_disk_file() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_dir = dir.path().join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        let disk_path = mcp_dir.join("slack.json");
        std::fs::write(&disk_path, r#"{"command":"npx","args":[],"enabled":false}"#).unwrap();
        let _args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
        cmd_unregister(unreg("mcp", "slack", dir.path())).unwrap();
        assert!(!disk_path.exists(), "mcp/slack.json should be deleted");
    }

    #[test]
    fn cmd_register_plugin_creates_directory_and_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{"model":"anthropic/claude-sonnet-4-6"}"#,
        )
        .unwrap();
        // Create a dummy .wasm file to register
        let wasm_path = dir.path().join("myfmt.wasm");
        std::fs::write(&wasm_path, b"").unwrap();
        let _args: Vec<String> = [
            "plugin",
            "myfmt",
            "--wasm",
            wasm_path.to_str().unwrap(),
            "--description",
            "A formatter",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        cmd_register(reg_flags(
            "plugin",
            "myfmt",
            dir.path(),
            &[
                "--wasm",
                wasm_path.to_str().unwrap(),
                "--description",
                "A formatter",
            ],
        ))
        .unwrap();
        let plugin_dir = dir.path().join("plugins").join("myfmt");
        assert!(plugin_dir.is_dir());
        assert!(
            plugin_dir.join("myfmt.wasm").exists(),
            "wasm should be copied"
        );
        let md = std::fs::read_to_string(plugin_dir.join("PLUGIN.md")).unwrap();
        assert!(md.contains("name: myfmt"));
        assert!(md.contains("enabled: false"));
        assert!(md.contains("A formatter"));
    }

    #[test]
    fn cmd_enable_plugin_sets_flag_in_plugin_md() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("plugins").join("myfmt");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("PLUGIN.md"),
            "---\nname: myfmt\nwasm: myfmt.wasm\nenabled: false\n---\n# docs\n",
        )
        .unwrap();
        let _args: Vec<String> = ["plugin", "myfmt"].iter().map(|s| s.to_string()).collect();
        cmd_enable_disable_inner(dir.path(), "plugin", "myfmt", true).unwrap();
        let md = std::fs::read_to_string(plugin_dir.join("PLUGIN.md")).unwrap();
        assert!(md.contains("enabled: true"), "got: {md}");
        assert!(
            !md.contains("enabled: false"),
            "old value should be gone: {md}"
        );
    }

    #[test]
    fn cmd_unregister_plugin_removes_directory() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("plugins").join("myfmt");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("PLUGIN.md"),
            "---\nname: myfmt\nwasm: myfmt.wasm\nenabled: false\n---\n",
        )
        .unwrap();
        let _args: Vec<String> = ["plugin", "myfmt"].iter().map(|s| s.to_string()).collect();
        cmd_unregister(unreg("plugin", "myfmt", dir.path())).unwrap();
        assert!(!plugin_dir.exists(), "plugins/myfmt/ should be deleted");
    }

    #[test]
    fn set_frontmatter_bool_updates_enabled() {
        let content = "---\nname: foo\nenabled: false\n---\n# body\ncontent\n";
        let result = set_frontmatter_bool(content, "enabled", true).unwrap();
        assert!(result.contains("enabled: true"), "got: {result}");
        assert!(result.contains("# body"), "body should be preserved");
    }
}
