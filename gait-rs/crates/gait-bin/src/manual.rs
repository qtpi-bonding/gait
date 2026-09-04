use clap::{Command, Parser, Subcommand};
use gait_repl::repl::commands::ReplCommand;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Which binary the manual is being generated for. Determines which
/// guides/config fields are visible (via each guide/field's `requires`
/// capability list) and which program name command paths render under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualFlavor {
    Full,
    Min,
}

impl ManualFlavor {
    fn capabilities(self) -> &'static [&'static str] {
        match self {
            ManualFlavor::Full => FULL_CAPABILITIES,
            ManualFlavor::Min => MIN_CAPABILITIES,
        }
    }
}

/// Capabilities present in the full `gait` build. `extensions` is an
/// umbrella over `mcp`+`plugins`+`code-mode` (all compiled into `full`);
/// treated here as its own plain literal member rather than derived.
/// `skills` is deliberately absent — it is not a gated feature; it is
/// available in every build.
const FULL_CAPABILITIES: &[&str] = &[
    "workflow",
    "rich-tui",
    "mcp",
    "code-mode",
    "local",
    "extensions",
];
const MIN_CAPABILITIES: &[&str] = &[];

const CONFIG_FIELDS: &[(&str, &str, &[&str])] = &[
    (
        "inference",
        "The selected model, e.g. \"anthropic/claude-sonnet-4-6\".",
        &[],
    ),
    ("allow", "Tool-name / pattern allowlist.", &[]),
    ("deny", "Tool-name / pattern denylist.", &[]),
    (
        "tools",
        "Config-defined shell-command tools.",
        &["extensions"],
    ),
    (
        "hooks",
        "Lifecycle hook definitions (event, tool filter, shell command).",
        &["extensions"],
    ),
    (
        "output_schema_retries",
        "Retry budget for structured_done schema validation.",
        &[],
    ),
    (
        "max_iter",
        "Max turn iterations before giving up; defaults to 100 when absent.",
        &[],
    ),
    (
        "shell_timeout_secs",
        "Wall-clock timeout for a foreground shell command.",
        &[],
    ),
    (
        "tool_timeout_secs",
        "Wall-clock timeout for a single ripgrep-backed grep/codesearch/ls subprocess.",
        &[],
    ),
    ("preview", "When to render edit/write previews.", &[]),
    (
        "result_schema",
        "Optional JSON Schema the final structured_done result must match.",
        &[],
    ),
    (
        "show_thinking",
        "Whether to render thinking/reasoning blocks in terminal output.",
        &[],
    ),
    (
        "sandbox",
        "Sandbox engagement and filesystem/network policy.",
        &[],
    ),
];

fn requires_met(requires: &[&str], flavor: ManualFlavor) -> bool {
    requires.iter().all(|r| flavor.capabilities().contains(r))
}

fn render_config_fields(out: &mut String, flavor: ManualFlavor) {
    out.push_str("\n### Config fields\n\n");
    for (name, desc, requires) in CONFIG_FIELDS {
        if requires_met(requires, flavor) {
            let _ = writeln!(out, "- `{name}` — {desc}");
        }
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

/// `gait-min manual` — a deliberately smaller command surface than full
/// `gait manual`. `print` writes the generated Markdown to stdout only; it
/// creates no files (not because skills are unavailable in Min — they are —
/// but because no future `install` path exists for Min yet). Every other
/// action is an error; `install` is intentionally absent from
/// `gait-min --help`.
#[derive(Debug, Parser)]
pub struct MinManualArgs {
    #[command(subcommand)]
    pub command: MinManualCommand,
}

#[derive(Debug, Subcommand)]
pub enum MinManualCommand {
    /// Print the generated model-facing manual to stdout.
    Print,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ManualManifest {
    #[serde(default)]
    pub guides: Vec<GuideSource>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GuideSource {
    pub title: String,
    /// Capabilities that must all be present in the selected `ManualFlavor`
    /// for this guide to render. Absent/empty means "always render."
    #[serde(default)]
    pub requires: Vec<String>,
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

/// Inputs to [`render_manual`]/[`render_manual_with_manifest`]: which
/// program name command paths render under, the live `clap::Command` tree,
/// the REPL command registry, and the flavor that gates guides/config
/// fields by capability.
pub struct ManualInput<'a> {
    pub program_name: &'a str,
    pub cli: Command,
    pub repl_commands: &'a [ReplCommand],
    pub flavor: ManualFlavor,
}

fn guide_visible(guide: &GuideSource, flavor: ManualFlavor) -> bool {
    let requires: Vec<&str> = guide.requires.iter().map(String::as_str).collect();
    requires_met(&requires, flavor)
}

/// Render the model-facing manual from the live CLI and REPL definitions,
/// using a caller-supplied manifest (used by tests to exercise custom
/// guides without touching the real `gait-manual/manual.yaml`).
fn render_manual_with_manifest(
    manifest: &ManualManifest,
    input: ManualInput<'_>,
) -> Result<String, ManualError> {
    let ManualInput {
        program_name,
        cli,
        repl_commands,
        flavor,
    } = input;
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
    for guide in manifest.guides.iter().filter(|g| guide_visible(g, flavor)) {
        writeln!(body, "\n## {}\n\n{}", guide.title, guide.body.trim())
            .expect("writing to String cannot fail");

        if !guide.cli_cmds.is_empty() {
            body.push_str("\n### CLI commands\n");
            for name in &guide.cli_cmds {
                let command = cli_index.get(name).ok_or_else(|| ManualError::UnknownCli {
                    guide: guide.title.clone(),
                    command: name.clone(),
                })?;
                render_cli_command(&mut body, program_name, name, command);
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
    render_config_fields(&mut body, flavor);
    while body.ends_with('\n') {
        body.pop();
    }
    body.push('\n');

    let hash = manual_source_hash(program_name, flavor, manifest, &cli, repl_commands);
    Ok(format!(
        "---\nname: gait-manual\ndescription: Explain how to use and configure Gait.\nmanual-source-hash: {hash}\n---\n\n{body}"
    ))
}

/// Render the model-facing manual for `input.flavor` from the real
/// `gait-manual/manual.yaml`.
pub fn render_manual(input: ManualInput<'_>) -> Result<String, ManualError> {
    let manifest = parse_manifest(include_str!("../../../gait-manual/manual.yaml"))?;
    render_manual_with_manifest(&manifest, input)
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
    let content = render_manual(ManualInput {
        program_name: "gait",
        cli: crate::cli(),
        repl_commands: gait_repl::repl::commands::COMMANDS,
        flavor: ManualFlavor::Full,
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    println!("installed → {}", path.display());
    Ok(())
}

/// `gait-min manual print` — writes the generated Markdown to stdout only.
/// Creates no files.
pub fn print(args: MinManualArgs, cli: Command, repl_commands: &[ReplCommand]) -> anyhow::Result<()> {
    let MinManualCommand::Print = args.command;
    let content = render_manual(ManualInput {
        program_name: "gait-min",
        cli,
        repl_commands,
        flavor: ManualFlavor::Min,
    })?;
    print!("{content}");
    Ok(())
}

fn manual_source_hash(
    program_name: &str,
    flavor: ManualFlavor,
    manifest: &ManualManifest,
    cli: &Command,
    repl_commands: &[ReplCommand],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(program_name.as_bytes());
    hasher.update(format!("{flavor:?}").as_bytes());
    let visible_guides: Vec<&GuideSource> = manifest
        .guides
        .iter()
        .filter(|g| guide_visible(g, flavor))
        .collect();
    hasher.update(serde_yaml::to_string(&visible_guides).expect("manual manifest serializes"));
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

fn render_cli_command(out: &mut String, program_name: &str, path: &str, command: &Command) {
    let _ = writeln!(out, "\n#### `{program_name} {path}`\n");
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
    use gait_repl::repl::commands::COMMANDS;
    use tempfile::TempDir;

    fn full_input(cli: Command) -> ManualInput<'static> {
        ManualInput {
            program_name: "gait",
            cli,
            repl_commands: COMMANDS,
            flavor: ManualFlavor::Full,
        }
    }

    #[test]
    fn renders_live_command_metadata() {
        let manifest = parse_manifest("guides:\n  - title: Sessions\n    cli_cmds: [sessions]\n    repl_cmds: [/new]\n    body: |\n      Manage sessions.\n").unwrap();
        let rendered = render_manual_with_manifest(&manifest, full_input(crate::cli())).unwrap();
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
            render_manual_with_manifest(&manifest, full_input(crate::cli())),
            Err(ManualError::UnknownCli { .. })
        ));
    }

    #[test]
    fn renders_config_field_reference() {
        let manifest = parse_manifest(
            "guides:\n  - title: Configuration\n    body: |\n      How gait.jsonc works.\n",
        )
        .unwrap();
        let rendered = render_manual_with_manifest(&manifest, full_input(crate::cli())).unwrap();
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
            CONFIG_FIELDS.iter().map(|(name, _, _)| *name).collect();
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

    // `install()` renders with `ManualFlavor::Full`, and the manifest's
    // "Workflows and extensions" guide references the `workflow` CLI
    // subcommand — which `crate::cli()` only compiles in when the
    // `workflow` Cargo feature is on. In practice this is always true for
    // shipping `gait` (full → workflow) and never exercised by `gait-min`
    // (which never calls `install()`/`crate::cli()`), but a bare
    // `--features min` build (no `workflow`) genuinely lacks the
    // subcommand, so gate this test to match where it's meaningful.
    #[cfg(feature = "workflow")]
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
