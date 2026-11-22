use gait_repl::repl::commands::ReplCommand;
use op_registry::manual::{FrontMatter, Guide, Manual, Section};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write;

use crate::ops::GaitContext;

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

/// Fields whose visibility depends on a build capability, beyond what a doc
/// comment can express. `tools`/`hooks` (config-defined shell tools / hooks)
/// only make sense to document when `extensions` (MCP/plugins/code-mode) is
/// compiled in — everything else is unconditionally visible.
const CONFIG_FIELD_REQUIRES: &[(&str, &[&str])] =
    &[("tools", &["extensions"]), ("hooks", &["extensions"])];

fn config_field_requires(name: &str) -> &'static [&'static str] {
    CONFIG_FIELD_REQUIRES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, r)| *r)
        .unwrap_or(&[])
}

fn requires_met(requires: &[&str], flavor: ManualFlavor) -> bool {
    requires.iter().all(|r| flavor.capabilities().contains(r))
}

/// Config-field descriptions come straight from `GaitConfig`'s own doc
/// comments via its live `JsonSchema` derive — the same "can't drift" source
/// CLI flags already get from `clap`'s `--help`. See `CONFIG_FIELD_REQUIRES`
/// for the one thing a schema can't carry (capability gating).
fn render_config_fields(out: &mut String, flavor: ManualFlavor) {
    let schema = schema_json(&schemars::schema_for!(gait_core::config::GaitConfig));
    render_schema_fields(out, &schema, &schema, "", &|name| {
        requires_met(config_field_requires(name), flavor)
    });
}

fn schema_json(schema: &schemars::schema::RootSchema) -> serde_json::Value {
    serde_json::to_value(schema).expect("schema serializes to JSON")
}

/// The definition a schema node refers to, plus the path suffix that reaches
/// its fields from the node (`[]` for array items, `.<key>` for map values).
fn schema_target<'a>(
    root: &'a serde_json::Value,
    node: &'a serde_json::Value,
) -> Option<(&'a serde_json::Value, &'static str)> {
    if let Some(r) = node.get("$ref").and_then(|r| r.as_str()) {
        let name = r.strip_prefix("#/definitions/")?;
        return Some((root.get("definitions")?.get(name)?, ""));
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(found) = node
            .get(key)
            .and_then(|v| v.as_array())
            .and_then(|items| items.iter().find_map(|i| schema_target(root, i)))
        {
            return Some(found);
        }
    }
    if let Some(items) = node.get("items") {
        return schema_target(root, items).map(|(d, _)| (d, "[]"));
    }
    if let Some(extra) = node.get("additionalProperties") {
        return schema_target(root, extra).map(|(d, _)| (d, ".<name>"));
    }
    None
}

/// Print one bullet per property of `node` (an object schema) with its doc
/// comment, then recurse into object-typed properties under a dotted path.
/// Descriptions are the Rust doc comments of the derived `JsonSchema` types.
fn render_schema_fields(
    out: &mut String,
    root: &serde_json::Value,
    node: &serde_json::Value,
    prefix: &str,
    visible: &dyn Fn(&str) -> bool,
) {
    let Some(props) = node.get("properties").and_then(|p| p.as_object()) else {
        return;
    };
    for (name, sub) in props {
        if prefix.is_empty() && !visible(name) {
            continue;
        }
        let desc = sub
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or_default();
        let path = format!("{prefix}{name}");
        let _ = writeln!(out, "- `{path}` — {desc}");
        if let Some((def, suffix)) = schema_target(root, sub) {
            render_schema_fields(out, root, def, &format!("{path}{suffix}."), &|_| true);
        }
    }
}

fn render_agent_file_format(out: &mut String) {
    out.push_str("YAML frontmatter, then the markdown body (the agent's role prompt).\n\n");
    let agent = schema_json(&schemars::schema_for!(
        gait_types::frontmatter::AgentFrontmatter
    ));
    render_schema_fields(out, &agent, &agent, "", &|_| true);
}

fn render_skill_file_format(out: &mut String) {
    out.push_str("YAML frontmatter, then the markdown body (the skill's instructions; a skill with an empty body is ignored).\n\n");
    let skill = schema_json(&schemars::schema_for!(
        gait_types::frontmatter::SkillFrontmatter
    ));
    render_schema_fields(out, &skill, &skill, "", &|_| true);
}

/// One authored file format, rendered from its proto message: the field list
/// (name, type, cardinality, doc comment) comes from `proto/log.proto` via the
/// embedded descriptor set, so it cannot drift from what the loader accepts.
fn render_message_fields(out: &mut String, full_name: &str, skip: &[&str]) {
    use gait_proto::schema_doc::{enum_values, message_doc, Label};
    let doc = message_doc(full_name).unwrap_or_else(|| panic!("{full_name} in descriptor set"));
    for field in doc
        .fields
        .iter()
        .filter(|f| !skip.contains(&f.name.as_str()))
    {
        let label = match field.label {
            Label::Optional => ", optional",
            Label::Repeated => ", repeated",
            Label::Singular => "",
        };
        let oneof = field
            .oneof
            .as_ref()
            .map(|o| format!(", one of `{o}`"))
            .unwrap_or_default();
        let package = full_name.rsplit_once('.').map_or("", |(p, _)| p);
        let values = enum_values(&format!("{package}.{}", field.ty))
            .map(|v| {
                format!(
                    "Values: {}.",
                    v.iter()
                        .map(|n| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .unwrap_or_default();
        let sep = if field.doc.is_empty() && values.is_empty() {
            ""
        } else {
            " — "
        };
        let _ = writeln!(
            out,
            "- `{}` ({}{label}{oneof}){sep}{}{}{values}",
            field.name,
            field.ty,
            field.doc,
            if field.doc.is_empty() || values.is_empty() {
                ""
            } else {
                " "
            }
        );
    }
}

const INFERENCE_EXAMPLE: &str = "model: \"anthropic/claude-sonnet-4-6\"\ncompaction_threshold_tokens: 128000\nllm_params { anthropic { effort: \"high\" } }\n";
const MODE_EXAMPLE: &str = "---\ndescription: Read-only review\nallow: [read, glob]\ndeny: [\"shell:git push *\"]\n---\nReview the code. Do not edit files.\n";

fn render_inference_format(out: &mut String, flavor: ManualFlavor) {
    out.push_str("A protobuf text-format `gait.log.v1.Inference`. The profile name is the directory name; do not set `name`.\n\n```\n");
    out.push_str(INFERENCE_EXAMPLE);
    out.push_str("```\n\n");
    render_message_fields(out, "gait.log.v1.Inference", &["name"]);
    out.push_str("\n`llm_params`:\n\n");
    render_message_fields(out, "gait.log.v1.LlmParams", &[]);
    let mut providers = vec![
        ("anthropic", "gait.log.v1.AnthropicLlmParams"),
        ("openai", "gait.log.v1.OpenAILlmParams"),
        ("gemini", "gait.log.v1.GeminiLlmParams"),
    ];
    providers.push(("anthropic.thinking", "gait.log.v1.AnthropicThinking"));
    providers.push(("gemini.thinking", "gait.log.v1.GeminiThinking"));
    if requires_met(&["local"], flavor) {
        providers.push(("local", "gait.log.v1.LocalLlmParams"));
    }
    for (name, message) in providers {
        let _ = write!(out, "\n`llm_params.{name}`:\n\n");
        render_message_fields(out, message, &[]);
    }
}

fn render_mode_format(out: &mut String) {
    out.push_str("Markdown with YAML frontmatter. The frontmatter keys below are `gait.log.v1.Mode` fields; the markdown body is the steer.\n\n```\n");
    out.push_str(MODE_EXAMPLE);
    out.push_str("```\n\n");
    render_message_fields(out, "gait.log.v1.Mode", &["name", "steer"]);
    out.push_str("\n`classify_policy`:\n\n");
    render_message_fields(out, "gait.log.v1.ClassifyPolicy", &[]);
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
    #[error("guide '{guide}' references unknown command '{command}'")]
    UnknownCommand { guide: String, command: String },
    #[error("guide '{guide}' references unknown REPL command '{command}'")]
    UnknownRepl { guide: String, command: String },
    #[error("guide '{0}' has an empty title")]
    EmptyTitle(String),
    #[error("{0}")]
    Render(String),
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

fn guide_visible(guide: &GuideSource, flavor: ManualFlavor) -> bool {
    let requires: Vec<&str> = guide.requires.iter().map(String::as_str).collect();
    requires_met(&requires, flavor)
}

/// Whether `path` names an operation (or a group's default) of `registry`.
fn is_op_path(registry: &op_registry::Registry<GaitContext>, path: &str) -> bool {
    let chain: Vec<&str> = path.split_whitespace().collect();
    registry.descriptors().iter().any(|d| {
        d.path.iter().map(String::as_str).eq(chain.iter().copied())
            || (d.group_default
                && d.noun_path()
                    .iter()
                    .map(String::as_str)
                    .eq(chain.iter().copied()))
    })
}

/// The help of a command that is not an operation: the turn (the empty path)
/// or an extra command such as `workflow`.
fn other_command_help(
    registry: &op_registry::Registry<GaitContext>,
    path: &str,
) -> Option<(String, String)> {
    use clap::CommandFactory;
    let bin = registry.bin();
    if path.is_empty() {
        let mut turn = crate::ops::turn::TurnArgs::command().name(bin);
        let help = turn.render_long_help().to_string();
        return Some((format!("`{bin}`"), help));
    }
    let mut root = op_registry::cli::command(registry);
    root.build();
    let mut command = root.find_subcommand(path)?.clone();
    Some((
        format!("`{bin} {path}`"),
        command.render_long_help().to_string(),
    ))
}

fn push_repl_command(out: &mut String, command: &ReplCommand) {
    let _ = write!(out, "- `{}` — {}", command.usage, command.desc);
    if !command.aliases.is_empty() {
        let _ = write!(out, " (aliases: {})", command.aliases.join(", "));
    }
    out.push('\n');
}

/// Builds the op-registry manual for `registry` from `gait-manual/manual.yaml`
/// plus the reference sections (config fields and authored file formats).
/// The commands themselves are rendered by the library from `registry`; this
/// adds gait's guides, the REPL commands and the help of the commands that are
/// not operations (the turn, `workflow`).
pub fn manual_from(
    registry: &op_registry::Registry<GaitContext>,
    manifest: &ManualManifest,
    flavor: ManualFlavor,
    repl_commands: &[ReplCommand],
) -> Result<Manual, ManualError> {
    let mut guides = Vec::new();
    for source in manifest.guides.iter().filter(|g| guide_visible(g, flavor)) {
        let mut body = source.body.trim().to_owned();
        let mut commands = Vec::new();
        for path in &source.cli_cmds {
            if !path.is_empty() && is_op_path(registry, path) {
                commands.push(path.clone());
            } else if let Some((title, help)) = other_command_help(registry, path) {
                let help = help
                    .lines()
                    .map(str::trim_end)
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = write!(
                    body,
                    "\n\n#### {title}\n\n```text\n{}\n```",
                    help.trim_end()
                );
            } else {
                return Err(ManualError::UnknownCommand {
                    guide: source.title.clone(),
                    command: path.clone(),
                });
            }
        }
        if !source.repl_cmds.is_empty() {
            body.push_str("\n\n#### REPL commands\n\n");
            for name in &source.repl_cmds {
                let command = repl_commands
                    .iter()
                    .find(|c| c.slug == name || c.aliases.contains(&name.as_str()))
                    .ok_or_else(|| ManualError::UnknownRepl {
                        guide: source.title.clone(),
                        command: name.clone(),
                    })?;
                push_repl_command(&mut body, command);
            }
            body.truncate(body.trim_end().len());
        }
        guides.push(Guide {
            title: source.title.clone(),
            body,
            commands,
        });
    }

    let reference = |title: &str, fill: &dyn Fn(&mut String)| {
        let mut body = String::new();
        fill(&mut body);
        Guide {
            title: title.to_owned(),
            body: body.trim().to_owned(),
            commands: Vec::new(),
        }
    };
    let reference_guides = vec![
        reference("Config fields", &|out| render_config_fields(out, flavor)),
        reference(
            "Inference profile: `inferences/<name>/inference.textproto`",
            &|out| render_inference_format(out, flavor),
        ),
        reference("Mode: `modes/<name>/MODE.md`", &render_mode_format),
        reference(
            "Agent file: `agents/<name>/AGENT.md`",
            &render_agent_file_format,
        ),
        reference(
            "Skill file: `skills/<name>/SKILL.md`",
            &render_skill_file_format,
        ),
    ];
    Ok(Manual {
        front_matter: Some(FrontMatter {
            name: "gait-manual".to_owned(),
            description: "Explain how to use and configure Gait.".to_owned(),
        }),
        sections: vec![
            Section {
                title: "Guides".to_owned(),
                guides,
            },
            Section {
                title: "Reference".to_owned(),
                guides: reference_guides,
            },
        ],
    })
}

/// [`manual_from`] with the real `gait-manual/manual.yaml`.
pub fn manual(
    registry: &op_registry::Registry<GaitContext>,
    flavor: ManualFlavor,
    repl_commands: &[ReplCommand],
) -> Result<Manual, ManualError> {
    let manifest = parse_manifest(include_str!("../../../gait-manual/manual.yaml"))?;
    manual_from(registry, &manifest, flavor, repl_commands)
}

/// The Markdown manual of `registry`: the guides, the reference and every
/// command, with the commands' help taken from the registry.
pub fn render(
    registry: &op_registry::Registry<GaitContext>,
    flavor: ManualFlavor,
    repl_commands: &[ReplCommand],
) -> Result<String, ManualError> {
    let manual = manual(registry, flavor, repl_commands)?;
    op_registry::manual::render(registry, &manual)
        .map_err(|e| ManualError::Render(format!("{e:#}")))
}

/// The manual of the full `gait`.
pub fn render_full() -> Result<String, ManualError> {
    render(
        &crate::ops::registry(),
        ManualFlavor::Full,
        gait_repl::repl::commands::COMMANDS,
    )
}

/// The text `manual install` writes: [`render`] with a `manual-source-hash`
/// line in the front matter, the hash of everything that follows it, so a
/// stale installed copy is recognizable.
pub fn install_text(
    registry: &op_registry::Registry<GaitContext>,
    flavor: ManualFlavor,
    repl_commands: &[ReplCommand],
) -> Result<String, ManualError> {
    let text = render(registry, flavor, repl_commands)?;
    let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
    Ok(text.replacen(
        "\n---\n",
        &format!("\nmanual-source-hash: {hash}\n---\n"),
        1,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_repl::repl::commands::COMMANDS;
    use op_registry::Registry;

    fn registry() -> Registry<GaitContext> {
        crate::ops::registry()
    }

    fn render_with(manifest_yaml: &str) -> Result<String, ManualError> {
        let manifest = parse_manifest(manifest_yaml)?;
        let registry = registry();
        let manual = manual_from(&registry, &manifest, ManualFlavor::Full, COMMANDS)?;
        op_registry::manual::render(&registry, &manual)
            .map_err(|e| ManualError::Render(format!("{e:#}")))
    }

    #[cfg(feature = "full")]
    #[test]
    fn manual_documents_nested_config_and_markdown_frontmatter() {
        let rendered = render_full().unwrap();
        for needle in [
            "`sandbox.deny.read`",
            "`sandbox.shell_whitelist[].pattern`",
            "`sandbox.tools.<name>.reads`",
            "`hooks[].event`",
            "### Agent file: `agents/<name>/AGENT.md`",
            "`skills` —",
            "`model_invocable` —",
        ] {
            assert!(rendered.contains(needle), "manual is missing {needle}");
        }
    }

    #[cfg(feature = "full")]
    #[test]
    fn documented_examples_load() {
        let inference = gait_proto::textproto::parse_inference_text(INFERENCE_EXAMPLE).unwrap();
        assert_eq!(inference.compaction_threshold_tokens, 128000);
        let dir = tempfile::tempdir().unwrap();
        let mode_dir = dir.path().join("modes/review");
        std::fs::create_dir_all(&mode_dir).unwrap();
        std::fs::write(mode_dir.join("MODE.md"), MODE_EXAMPLE).unwrap();
        let mode = gait_state::modes::load_mode(dir.path(), "review").unwrap();
        assert_eq!(mode.allow, vec!["read", "glob"]);
        assert_eq!(mode.steer, "Review the code. Do not edit files.");
    }

    /// `docs/manual.md` is the published copy of `gait manual install`. This
    /// fails when a flag, command or guide changes without regenerating it;
    /// `GAIT_UPDATE_MANUAL=1 cargo test -p gait-bin committed_manual` (or
    /// `make manual`) rewrites the file.
    #[cfg(feature = "full")]
    #[test]
    fn committed_manual_is_current() {
        let rendered = render_full().unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../docs/manual.md");
        if std::env::var_os("GAIT_UPDATE_MANUAL").is_some() {
            std::fs::write(&path, &rendered).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == rendered,
            "docs/manual.md is out of date: run `make -C gait-rs manual` and commit the result"
        );
    }

    /// The retired spellings must not survive anywhere in the generated manual.
    #[cfg(feature = "full")]
    #[test]
    fn manual_has_no_retired_spellings() {
        let rendered = render_full().unwrap();
        assert!(
            !rendered.contains("--agent-dir"),
            "retired --agent-dir flag in the manual"
        );
        assert!(
            !rendered.contains("GAIT_AGENT_DIR"),
            "retired GAIT_AGENT_DIR in the manual"
        );
        assert!(
            !rendered.contains("agent dir"),
            "retired \"agent dir\" wording in the manual"
        );
        assert!(
            !rendered.contains("run error"),
            "retired \"run error\" in the manual"
        );
    }

    #[test]
    fn renders_live_command_metadata() {
        let rendered = render_with("guides:\n  - title: Sessions\n    cli_cmds: [sessions]\n    repl_cmds: [/new]\n    body: |\n      Manage sessions.\n").unwrap();
        assert!(
            rendered.contains("### gait sessions"),
            "the command help comes from the registry"
        );
        assert!(rendered.contains("Commands: `sessions`"));
        assert!(rendered.contains("`/new`"));
        assert!(rendered.contains("Start a fresh session"));
    }

    #[test]
    fn the_turn_and_workflow_are_documented_from_their_own_help() {
        let rendered = render_with(
            "guides:\n  - title: Turn\n    cli_cmds: [\"\"]\n    body: |\n      Tasks.\n",
        )
        .unwrap();
        assert!(
            rendered.contains("--auto-approve"),
            "the turn flags are in the manual"
        );
        #[cfg(feature = "workflow")]
        {
            let rendered = render_with(
                "guides:\n  - title: W\n    cli_cmds: [workflow]\n    body: |\n      Flows.\n",
            )
            .unwrap();
            assert!(rendered.contains("#### `gait workflow`"));
        }
    }

    #[test]
    fn rejects_unknown_command_references() {
        let result = render_with(
            "guides:\n  - title: Bad\n    cli_cmds: [does-not-exist]\n    body: |\n      x\n",
        );
        assert!(matches!(result, Err(ManualError::UnknownCommand { .. })));
    }

    #[test]
    fn rejects_unknown_repl_references() {
        let result = render_with(
            "guides:\n  - title: Bad\n    repl_cmds: [/does-not-exist]\n    body: |\n      x\n",
        );
        assert!(matches!(result, Err(ManualError::UnknownRepl { .. })));
    }

    #[test]
    fn renders_config_field_reference() {
        let rendered = render_with(
            "guides:\n  - title: Configuration\n    body: |\n      How gait.jsonc works.\n",
        )
        .unwrap();
        assert!(rendered.contains("### Config fields"));
        assert!(rendered.contains("`inference`"));
        assert!(rendered.contains("`allow`"));
    }

    #[test]
    fn install_text_carries_a_hash_in_the_front_matter() {
        let text = install_text(&registry(), ManualFlavor::Full, COMMANDS).unwrap();
        let head = text.split("\n---\n").next().unwrap();
        assert!(head.starts_with("---\nname: gait-manual\n"), "{head}");
        assert!(head.contains("\nmanual-source-hash: "), "{head}");
        assert_eq!(
            text,
            install_text(&registry(), ManualFlavor::Full, COMMANDS).unwrap()
        );
    }

    #[test]
    fn config_fields_table_covers_every_gaitconfig_field() {
        // Exhaustive struct literal: naming every field forces a compile
        // error here the moment `GaitConfig` gains or loses one, so this
        // list can't silently drift from the real struct.
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
        // Descriptions come from `GaitConfig`'s own doc comments via its
        // `JsonSchema` derive, so the failure mode this test guards against
        // is "field exists but has no doc comment".
        let schema = schemars::schema_for!(gait_core::config::GaitConfig);
        let object = schema.schema.object.as_ref().unwrap();
        let undocumented: Vec<&str> = all_fields
            .iter()
            .filter(|field| {
                let desc = object
                    .properties
                    .get(**field)
                    .and_then(|s| s.clone().into_object().metadata)
                    .and_then(|m| m.description);
                !matches!(desc, Some(d) if !d.is_empty())
            })
            .copied()
            .collect();
        assert!(
            undocumented.is_empty(),
            "GaitConfig field(s) missing a doc comment (used as the manual description): {undocumented:?}"
        );
    }
}
