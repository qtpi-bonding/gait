use clap::Command;
use gait_repl::repl::commands::{ReplCommand, COMMANDS};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct ManualManifest {
    #[serde(default)]
    pub guides: Vec<GuideSource>,
}

#[derive(Debug, Deserialize)]
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
    let cli_index = cli_index(cli);
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

    let mut out = String::from(
        "---\nname: gait-manual\ndescription: Explain how to use and configure Gait.\nmanual-source-hash: ",
    );
    out.push_str(&manual_source_hash());
    out.push_str("\n---\n\n# Gait Manual\n");
    for guide in &manifest.guides {
        writeln!(out, "\n## {}\n\n{}", guide.title, guide.body.trim())
            .expect("writing to String cannot fail");

        if !guide.cli_cmds.is_empty() {
            out.push_str("\n### CLI commands\n");
            for name in &guide.cli_cmds {
                let command = cli_index.get(name).ok_or_else(|| ManualError::UnknownCli {
                    guide: guide.title.clone(),
                    command: name.clone(),
                })?;
                render_cli_command(&mut out, name, command);
            }
        }

        if !guide.repl_cmds.is_empty() {
            out.push_str("\n### REPL commands\n");
            for name in &guide.repl_cmds {
                let command =
                    repl_index
                        .get(name.as_str())
                        .ok_or_else(|| ManualError::UnknownRepl {
                            guide: guide.title.clone(),
                            command: name.clone(),
                        })?;
                render_repl_command(&mut out, command);
            }
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    Ok(out)
}

pub fn render_default_skill() -> Result<String, ManualError> {
    let manifest = parse_manifest(include_str!("../../../gait-manual/manual.yaml"))?;
    render_skill(&manifest, gait_bin::cli(), COMMANDS)
}

/// Hash the inputs that can change generated manual content. This is shared
/// conceptually with the core build check; keep the list narrow enough that
/// unrelated implementation changes do not force regeneration.
pub fn manual_source_hash() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("gait workspace root");
    hash_sources(root)
}

pub fn hash_sources(root: &Path) -> String {
    let mut files = vec![root.join("gait-manual/manual.yaml")];
    for relative in [
        "crates/gait-mangen/src",
        "crates/gait-bin/src",
        "crates/gait-cli/src",
        "crates/gait-repl/src/repl",
    ] {
        collect_rust_files(&root.join(relative), &mut files);
    }
    files.sort();

    let mut hasher = Sha256::new();
    for path in files {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(
            std::fs::read(&path)
                .unwrap_or_else(|e| panic!("read manual source {}: {e}", path.display())),
        );
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn collect_rust_files(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_file() {
        if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path.to_path_buf());
        }
        return;
    }
    for entry in std::fs::read_dir(path)
        .unwrap_or_else(|e| panic!("read manual source directory {}: {e}", path.display()))
        .flatten()
    {
        collect_rust_files(&entry.path(), files);
    }
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

    #[test]
    fn renders_live_command_metadata() {
        let manifest = parse_manifest(
            r#"
guides:
  - title: Sessions
    cli_cmds: [sessions]
    repl_cmds: [/new]
    body: |
      Manage sessions.
"#,
        )
        .unwrap();
        let rendered = render_skill(&manifest, gait_bin::cli(), COMMANDS).unwrap();
        assert!(rendered.contains("#### `gait sessions`"));
        assert!(rendered.contains("#### `/new`"));
        assert!(rendered.contains("Start a fresh session"));
    }

    #[test]
    fn rejects_unknown_command_references() {
        let manifest = parse_manifest(
            "guides:\n  - title: Bad\n    cli_cmds: [does-not-exist]\n    body: |\n      x\n",
        )
        .unwrap();
        assert!(matches!(
            render_skill(&manifest, gait_bin::cli(), COMMANDS),
            Err(ManualError::UnknownCli { .. })
        ));
    }

    #[test]
    fn help_environment_values_are_stable() {
        assert_eq!(
            normalize_help_env_values("  --gait-dir [env: GAIT_DIR=/private/user/gait]"),
            "  --gait-dir [env: GAIT_DIR]"
        );
    }

    #[test]
    fn checked_in_skill_matches_generator() {
        assert_eq!(
            render_default_skill().unwrap(),
            include_str!("../../../gait-manual/SKILL.md")
        );
    }
}
