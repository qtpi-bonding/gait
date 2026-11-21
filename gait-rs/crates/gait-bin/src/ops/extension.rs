//! Extension ops: register, enable and load the MCP servers, skills and
//! plugins gait can use. Thin over `gait_ops::extension`.
use super::{infra, GaitContext};
use clap::ValueEnum;
use op_registry::{CallOptions, OpParams, Ref, UsageError};
use std::collections::BTreeMap;

/// What kind of extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    /// A Model Context Protocol server.
    Mcp,
    /// A skill: a SKILL.md of instructions.
    Skill,
    /// A WebAssembly plugin.
    Plugin,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Mcp => "mcp",
            Kind::Skill => "skill",
            Kind::Plugin => "plugin",
        }
    }

    fn ext(self) -> gait_ops::extension::ExtKind {
        use gait_ops::extension::ExtKind;
        match self {
            Kind::Mcp => ExtKind::Mcp,
            Kind::Skill => ExtKind::Skill,
            Kind::Plugin => ExtKind::Plugin,
        }
    }
}

/// Where a registration or removal applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Scope {
    /// The gait directory, for every session (the default).
    Global,
    /// The agent file named by `--agent`.
    Agent,
    /// One session's log, named by `--session`.
    Session,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Agent => "agent",
            Scope::Session => "session",
        }
    }
}

output! {
    /// An extension that was registered.
    pub struct ExtensionRegistered {
        pub kind: String,
        pub name: String,
        /// `global`, `agent` or `session`.
        pub scope: String,
        /// The file that was written.
        pub written_to: String,
        /// What to run next; empty when nothing is needed.
        pub hint: String,
        /// Always `registered`.
        pub action: String,
    }
}

output! {
    /// An extension that was unregistered.
    pub struct ExtensionUnregistered {
        pub kind: String,
        pub name: String,
        pub scope: String,
        pub written_to: String,
        /// Always `unregistered`.
        pub action: String,
    }
}

output! {
    /// An extension switched on or off in the gait directory.
    pub struct ExtensionToggled {
        pub kind: String,
        pub name: String,
        /// `enabled` or `disabled`.
        pub action: String,
    }
}

output! {
    /// An extension loaded into or unloaded from one session.
    pub struct ExtensionLoaded {
        /// The session id.
        pub session: String,
        pub kind: String,
        pub name: String,
        /// `loaded` or `unloaded`.
        pub action: String,
    }
}

fn validate_register(p: &ExtensionRegisterParams) -> Result<(), UsageError> {
    match p.kind {
        Kind::Mcp => {
            if p.url.is_none() && p.command.is_none() {
                return Err(UsageError::AtLeastOne {
                    group: "url or command".into(),
                });
            }
        }
        Kind::Skill => {
            if p.description.is_none() {
                return Err(UsageError::MissingField {
                    name: "description".into(),
                });
            }
            if p.content.is_none() && p.path.is_none() {
                return Err(UsageError::AtLeastOne {
                    group: "content or path".into(),
                });
            }
        }
        Kind::Plugin => {
            if p.wasm.is_none() {
                return Err(UsageError::MissingField {
                    name: "wasm".into(),
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, OpParams)]
#[op(validate = validate_register)]
pub struct ExtensionRegisterParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or daemon.
    #[op(subject = "extension", id_only, absent_ok)]
    pub id: Ref,
    /// The kind of extension.
    #[op(value_enum)]
    pub kind: Kind,
    /// The URL of an MCP server that is already running.
    #[op(one_of_optional = "mcp_source")]
    pub url: Option<String>,
    /// The program that starts an MCP server.
    #[op(one_of_optional = "mcp_source")]
    pub command: Option<String>,
    /// The arguments for `--command`, as one string split on spaces.
    #[op(allow_hyphen)]
    pub args: Option<String>,
    /// A one-line description; required when registering a skill.
    #[op(allow_hyphen)]
    pub description: Option<String>,
    /// The text of a skill, given inline.
    #[op(allow_hyphen)]
    #[op(one_of_optional = "skill_source")]
    pub content: Option<String>,
    /// A file to read a skill from.
    #[op(one_of_optional = "skill_source")]
    pub path: Option<String>,
    /// The `.wasm` file of a plugin.
    pub wasm: Option<String>,
    /// An environment variable for the MCP server, as `KEY=value` (repeatable).
    pub env: BTreeMap<String, String>,
    /// Where the change applies. When not given: `once` for `allow` and `unlock`, `global` for `register` and `unregister`.
    #[op(value_enum)]
    pub scope: Option<Scope>,
    /// The agent config to change, for `--scope agent`: its name for `unlock`, the path of its `.md` file for `register` and `unregister`.
    pub agent: Option<String>,
    /// The session id the change applies to.
    pub session: Option<String>,
}

#[derive(Debug, Clone, OpParams)]
pub struct ExtensionUnregisterParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or daemon.
    #[op(subject = "extension", id_only)]
    pub id: Ref,
    /// The kind of extension.
    #[op(value_enum)]
    pub kind: Kind,
    /// Where the change applies. When not given: `once` for `allow` and `unlock`, `global` for `register` and `unregister`.
    #[op(value_enum)]
    pub scope: Option<Scope>,
    /// The agent config to change, for `--scope agent`: its name for `unlock`, the path of its `.md` file for `register` and `unregister`.
    pub agent: Option<String>,
    /// The session id the change applies to.
    pub session: Option<String>,
}

#[derive(Debug, Clone, OpParams)]
pub struct ExtensionToggleParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or daemon.
    #[op(subject = "extension", id_only)]
    pub id: Ref,
    /// The kind of extension.
    #[op(value_enum)]
    pub kind: Kind,
}

#[derive(Debug, Clone, OpParams)]
pub struct ExtensionLoadParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or daemon.
    #[op(subject = "extension", id_only)]
    pub id: Ref,
    /// The kind of extension.
    #[op(value_enum)]
    pub kind: Kind,
    /// The session id the change applies to.
    pub session: String,
}

/// Register an MCP server, a skill or a plugin. An MCP server and a skill start disabled.
pub async fn register(
    ctx: &GaitContext,
    params: ExtensionRegisterParams,
    _opts: CallOptions,
) -> anyhow::Result<ExtensionRegistered> {
    let name = params.id.as_raw().to_owned();
    let outcome = gait_ops::extension::register(gait_ops::extension::RegisterParams {
        gait_dir: ctx.gait_dir()?,
        kind: params.kind.as_str().to_owned(),
        name: name.clone(),
        url: params.url,
        command: params.command,
        args: params.args,
        env: params.env,
        description: params.description,
        content: params.content,
        path: params.path,
        wasm: params.wasm,
        scope: params.scope.map(|s| s.as_str().to_owned()),
        agent: params.agent.map(Into::into),
        session: params.session,
    })
    .map_err(infra)?;
    Ok(ExtensionRegistered {
        kind: params.kind.as_str().into(),
        name,
        scope: outcome.scope.into(),
        written_to: outcome.written_to.display().to_string(),
        hint: outcome.hint,
        action: "registered".into(),
    })
}

/// Remove a registered extension.
pub async fn unregister(
    ctx: &GaitContext,
    params: ExtensionUnregisterParams,
    _opts: CallOptions,
) -> anyhow::Result<ExtensionUnregistered> {
    let name = params.id.as_raw().to_owned();
    let outcome = gait_ops::extension::unregister(gait_ops::extension::UnregisterParams {
        gait_dir: ctx.gait_dir()?,
        kind: params.kind.as_str().to_owned(),
        name: name.clone(),
        scope: params.scope.map(|s| s.as_str().to_owned()),
        agent: params.agent.map(Into::into),
        session: params.session,
    })
    .map_err(infra)?;
    Ok(ExtensionUnregistered {
        kind: params.kind.as_str().into(),
        name,
        scope: outcome.scope.into(),
        written_to: outcome.written_to.display().to_string(),
        action: "unregistered".into(),
    })
}

fn toggle(
    ctx: &GaitContext,
    params: ExtensionToggleParams,
    on: bool,
) -> anyhow::Result<ExtensionToggled> {
    let name = params.id.as_raw().to_owned();
    let p = gait_ops::extension::EnableParams {
        gait_dir: ctx.gait_dir()?,
        kind: params.kind.as_str().to_owned(),
        name: name.clone(),
    };
    if on {
        gait_ops::extension::enable(p).map_err(infra)?;
    } else {
        gait_ops::extension::disable(p).map_err(infra)?;
    }
    Ok(ExtensionToggled {
        kind: params.kind.as_str().into(),
        name,
        action: if on { "enabled" } else { "disabled" }.into(),
    })
}

/// Switch an extension on in the gait directory's config, for sessions that start later.
pub async fn enable(
    ctx: &GaitContext,
    params: ExtensionToggleParams,
    _opts: CallOptions,
) -> anyhow::Result<ExtensionToggled> {
    toggle(ctx, params, true)
}

/// Switch an extension off in the gait directory's config.
pub async fn disable(
    ctx: &GaitContext,
    params: ExtensionToggleParams,
    _opts: CallOptions,
) -> anyhow::Result<ExtensionToggled> {
    toggle(ctx, params, false)
}

fn session_toggle(
    ctx: &GaitContext,
    params: ExtensionLoadParams,
    on: bool,
) -> anyhow::Result<ExtensionLoaded> {
    let name = params.id.as_raw().to_owned();
    let p = gait_ops::extension::LoadParams {
        gait_dir: ctx.gait_dir()?,
        session_id: params.session.clone(),
        kind: params.kind.ext(),
        name: name.clone(),
    };
    if on {
        gait_ops::extension::load(p).map_err(infra)?;
    } else {
        gait_ops::extension::unload(p).map_err(infra)?;
    }
    Ok(ExtensionLoaded {
        session: params.session,
        kind: params.kind.as_str().into(),
        name,
        action: if on { "loaded" } else { "unloaded" }.into(),
    })
}

/// Load an extension into one session from its next turn.
pub async fn load(
    ctx: &GaitContext,
    params: ExtensionLoadParams,
    _opts: CallOptions,
) -> anyhow::Result<ExtensionLoaded> {
    session_toggle(ctx, params, true)
}

/// Unload an extension from one session.
pub async fn unload(
    ctx: &GaitContext,
    params: ExtensionLoadParams,
    _opts: CallOptions,
) -> anyhow::Result<ExtensionLoaded> {
    session_toggle(ctx, params, false)
}
