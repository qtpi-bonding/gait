use clap::{Command, Parser, Subcommand};
use gait_repl::repl::commands::{ReplCommand, COMMANDS};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

const CONFIG_FIELDS: &[(&str, &str)] = &[
    (
        "inference",
        "The selected model, e.g. \"anthropic/claude-sonnet-4-6\".",
    ),
    ("allow", "Tool-name / pattern allowlist."),
    ("deny", "Tool-name / pattern denylist."),
    ("tools", "Config-defined shell-command tools."),
    (
        "hooks",
        "Lifecycle hook definitions (event, tool filter, shell command).",
    ),
    (
        "output_schema_retries",
        "Retry budget for structured_done schema validation.",
    ),
    (
        "max_iter",
        "Max turn iterations before giving up; defaults to 100 when absent.",
    ),
    (
        "shell_timeout_secs",
        "Wall-clock timeout for a foreground shell command.",
    ),
    (
        "tool_timeout_secs",
        "Wall-clock timeout for a single ripgrep-backed grep/codesearch/ls subprocess.",
    ),
    ("preview", "When to render edit/write previews."),
    (
        "result_schema",
        "Optional JSON Schema the final structured_done result must match.",
    ),
    (
        "show_thinking",
        "Whether to render thinking/reasoning blocks in terminal output.",
    ),
    (
        "sandbox",
        "Sandbox engagement and filesystem/network policy.",
    ),
];

fn render_config_fields(out: &mut String) {
    out.push_str("\n### Config fields\n\n");
    for (name, desc) in CONFIG_FIELDS {
        let _ = writeln!(out, "- `{name}` — {desc}");
    }
}

#[derive(Debug, Parser)]
pub struct ManualArgs {
    #[command(subcommand)]
    pub command: ManualCommand,
}

#[derive(Debug, Subcommand)]
pub enum ManualCommand {
    /// Generate the model-facing manual in the configured Gait directory.
    Install(ManualInstallArgs),
}

#[derive(Debug, Parser)]
pub struct ManualInstallArgs {
    /// Replace an existing generated manual.
    #[arg(long)]
    pub force: bool,
    /// Gait directory in which to install the manual.
    #[arg(long, short = 'd')]
    pub gait_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ManualManifest {
    #[serde(default)]
    pub guides: Vec<GuideSource>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GuideSource {
    pub title: String,
    #[serde(default)]
    pub cli_cmds: Vec<String>,
    #[serde(default)]
    pub repl_cmds: Vec<String>,
    pub body: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ManualError {
    #[error("invalid manual YAML: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("guide '{guide}' references unknown CLI command '{command}'")]
    UnknownCli { guide: String, command: String },
    #[error("guide '{guide}' references unknown REPL command '{command}'")]
    UnknownRepl { guide: String, command: String },
    #[error("guide '{0}' has an empty title")]
    EmptyTitle(String),
}

pub fn parse_manifest(yaml: &str) -> Result<ManualManifest, ManualError> {
    let manifest: ManualManifest = serde_yaml::from_str(yaml)?;
    for guide in &manifest.guides {
        if guide.title.trim().is_empty() {
            return Err(ManualError::EmptyTitle(guide.title.clone()));
        }
    }
    Ok(manifest)
}

/// Render the model-facing manual from the live CLI and REPL definitions.
pub fn render_skill(
    manifest: &ManualManifest,
    cli: Command,
    repl_commands: &[ReplCommand],
) -> Result<String, ManualError> {
    let cli_index = cli_index(cli.clone());
    let repl_index: BTreeMap<&str, &ReplCommand> = repl_commands
        .iter()
        .flat_map(|command| std::iter::once(command.slug).chain(command.aliases.iter().copied()))
        .filter_map(|name| {
            repl_commands
                .iter()
                .find(|command| command.slug == name || command.aliases.contains(&name))
                .map(|command| (name, command))
        })
        .collect();

    let mut body = String::from("# Gait Manual\n");
    for guide in &manifest.guides {
        writeln!(body, "\n## {}\n\n{}", guide.title, guide.body.trim())
            .expect("writing to String cannot fail");

        if !guide.cli_cmds.is_empty() {
            body.push_str("\n### CLI commands\n");
            for name in &guide.cli_cmds {
                let command = cli_index.get(name).ok_or_else(|| ManualError::UnknownCli {
                    guide: guide.title.clone(),
                    command: name.clone(),
                })?;
                render_cli_command(&mut body, name, command);
            }
        }

        if !guide.repl_cmds.is_empty() {
            body.push_str("\n### REPL commands\n");
            for name in &guide.repl_cmds {
                let command =
                    repl_index
                        .get(name.as_str())
                        .ok_or_else(|| ManualError::UnknownRepl {
                            guide: guide.title.clone(),
                            command: name.clone(),
                        })?;
                render_repl_command(&mut body, command);
            }
        }
    }
    render_config_fields(&mut body);
    while body.ends_with('\n') {
        body.pop();
    }
    body.push('\n');

    let hash = manual_source_hash(manifest, &cli, repl_commands);
    Ok(format!(
        "---\nname: gait-manual\ndescription: Explain how to use and configure Gait.\nmanual-source-hash: {hash}\n---\n\n{body}"
    ))
}

pub fn render_default_skill() -> Result<String, ManualError> {
    let manifest = parse_manifest(include_str!("../../../gait-manual/manual.yaml"))?;
    render_skill(&manifest, crate::cli(), COMMANDS)
}

pub fn install(args: ManualArgs, gait_dir: &Path) -> anyhow::Result<()> {
    let ManualCommand::Install(options) = args.command;
    let path = gait_dir.join("skills/gait-manual/SKILL.md");
    if path.exists() && !options.force {
        anyhow::bail!(
            "{} already exists; pass --force to regenerate it",
            path.display()
        );
    }
    let content = render_default_skill()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    println!("installed → {}", path.display());
    Ok(())
}

fn manual_source_hash(
    manifest: &ManualManifest,
    cli: &Command,
    repl_commands: &[ReplCommand],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(serde_yaml::to_string(manifest).expect("manual manifest serializes"));
    for command in cli_index(cli.clone()).values() {
        hasher.update(command.get_name().as_bytes());
        hasher.update(normalize_help_env_values(
            &command.clone().render_help().to_string(),
        ));
    }
    for command in repl_commands {
        hasher.update(command.slug.as_bytes());
        hasher.update(command.aliases.join(",").as_bytes());
        hasher.update(command.desc.as_bytes());
        hasher.update(command.usage.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn cli_index(root: Command) -> BTreeMap<String, Command> {
    let mut index = BTreeMap::new();
    for child in root.get_subcommands() {
        collect_cli_commands(child.clone(), String::new(), &mut index);
    }
    index
}

fn collect_cli_commands(command: Command, parent: String, index: &mut BTreeMap<String, Command>) {
    let path = if parent.is_empty() {
        command.get_name().to_string()
    } else {
        format!("{parent} {}", command.get_name())
    };
    if !command.is_hide_set() {
        index.insert(path.clone(), command.clone());
    }
    for child in command.get_subcommands() {
        collect_cli_commands(child.clone(), path.clone(), index);
    }
}

fn render_cli_command(out: &mut String, path: &str, command: &Command) {
    let _ = writeln!(out, "\n#### `gait {path}`\n");
    if let Some(about) = command.get_about() {
        let _ = writeln!(out, "{}\n", about);
    }
    let help = normalize_help_env_values(&command.clone().render_help().to_string())
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let _ = writeln!(out, "```text\n{}\n```", help.trim_end());
}

fn normalize_help_env_values(help: &str) -> String {
    help.lines()
        .map(|line| {
            let Some(start) = line.find("[env: ") else {
                return line.to_string();
            };
            let Some(end_offset) = line[start..].find(']') else {
                return line.to_string();
            };
            let end = start + end_offset;
            let field = &line[start + "[env: ".len()..end];
            let name = field.split('=').next().unwrap_or(field);
            format!("{}[env: {name}]{}", &line[..start], &line[end + 1..])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_repl_command(out: &mut String, command: &ReplCommand) {
    let _ = writeln!(out, "\n#### `{}`\n", command.usage);
    let _ = writeln!(out, "{}\n", command.desc);
    if !command.aliases.is_empty() {
        let _ = writeln!(out, "Aliases: {}\n", command.aliases.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn renders_live_command_metadata() {
        let manifest = parse_manifest("guides:\n  - title: Sessions\n    cli_cmds: [sessions]\n    repl_cmds: [/new]\n    body: |\n      Manage sessions.\n").unwrap();
        let rendered = render_skill(&manifest, crate::cli(), COMMANDS).unwrap();
        assert!(rendered.contains("#### `gait sessions`"));
        assert!(rendered.contains("#### `/new`"));
        assert!(rendered.contains("Start a fresh session"));
        assert!(rendered.contains("manual-source-hash: "));
    }

    #[test]
    fn rejects_unknown_command_references() {
        let manifest = parse_manifest(
            "guides:\n  - title: Bad\n    cli_cmds: [does-not-exist]\n    body: |\n      x\n",
        )
        .unwrap();
        assert!(matches!(
            render_skill(&manifest, crate::cli(), COMMANDS),
            Err(ManualError::UnknownCli { .. })
        ));
    }

    #[test]
    fn renders_config_field_reference() {
        let manifest = parse_manifest(
            "guides:\n  - title: Configuration\n    body: |\n      How gait.jsonc works.\n",
        )
        .unwrap();
        let rendered = render_skill(&manifest, crate::cli(), COMMANDS).unwrap();
        assert!(rendered.contains("### Config fields"));
        assert!(rendered.contains("`inference`"));
        assert!(rendered.contains("`allow`"));
    }

    #[test]
    fn config_fields_table_covers_every_gaitconfig_field() {
        let _exhaustive = gait_core::config::GaitConfig {
            inference: None,
            allow: vec![],
            deny: vec![],
            tools: vec![],
            hooks: vec![],
            output_schema_retries: 3,
            max_iter: None,
            shell_timeout_secs: 60,
            tool_timeout_secs: 30,
            preview: Default::default(),
            result_schema: None,
            show_thinking: false,
            sandbox: Default::default(),
        };
        let documented: std::collections::HashSet<&str> =
            CONFIG_FIELDS.iter().map(|(name, _)| *name).collect();
        let all_fields = [
            "inference",
            "allow",
            "deny",
            "tools",
            "hooks",
            "output_schema_retries",
            "max_iter",
            "shell_timeout_secs",
            "tool_timeout_secs",
            "preview",
            "result_schema",
            "show_thinking",
            "sandbox",
        ];
        let missing: Vec<&str> = all_fields
            .iter()
            .filter(|field| !documented.contains(**field))
            .copied()
            .collect();
        assert!(
            missing.is_empty(),
            "GaitConfig field(s) missing from CONFIG_FIELDS in manual.rs: {missing:?}"
        );
    }

    #[test]
    fn install_writes_manual_skill() {
        let dir = TempDir::new().unwrap();
        install(
            ManualArgs {
                command: ManualCommand::Install(ManualInstallArgs {
                    force: false,
                    gait_dir: None,
                }),
            },
            dir.path(),
        )
        .unwrap();
        assert!(dir.path().join("skills/gait-manual/SKILL.md").is_file());
    }
}
