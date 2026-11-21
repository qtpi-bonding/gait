//! Mode ops: the named permission modes of a gait directory, and choosing one
//! for a session. Thin over `gait_ops::mode`.
use super::{infra, require_active, GaitContext};
use op_registry::{CallOptions, OpParams, Ref};

output! {
    /// A named mode.
    pub struct ModeItem {
        pub name: String,
        /// Tool patterns the mode allows.
        pub allow: Vec<String>,
        /// Tool patterns the mode denies.
        pub deny: Vec<String>,
        /// Free-form steering text added to the prompt.
        pub steer: String,
        pub description: String,
    }
}

output! {
    /// A mode that was written.
    pub struct ModeAdded {
        pub name: String,
        pub allow: Vec<String>,
        pub deny: Vec<String>,
        /// Always `added`.
        pub action: String,
    }
}

output! {
    /// The mode a session now uses.
    pub struct ModeSelected {
        /// The session id.
        pub session: String,
        /// The mode name, `default` after `clear`.
        pub name: String,
    }
}

#[derive(Debug, Clone, OpParams)]
pub struct ModeListParams {}

/// A mode that must exist.
#[derive(Debug, Clone, OpParams)]
pub struct ModeRef {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or daemon.
    #[op(subject = "mode")]
    pub id: Ref,
}

#[derive(Debug, Clone, OpParams)]
pub struct ModeAddParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or daemon.
    #[op(subject = "mode", id_only, absent_ok)]
    pub id: Ref,
    /// A tool pattern the mode allows (repeatable).
    pub allow: Vec<String>,
    /// A tool pattern the mode denies (repeatable).
    pub deny: Vec<String>,
    /// Free-form steering text added to the prompt.
    #[op(allow_hyphen)]
    pub steer: Option<String>,
    /// A one-line description; required when registering a skill.
    #[op(allow_hyphen)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, OpParams)]
pub struct ModeSelectParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or daemon.
    #[op(subject = "mode")]
    pub id: Ref,
    /// The session id the change applies to.
    pub session: String,
}

#[derive(Debug, Clone, OpParams)]
pub struct ModeClearParams {
    /// The session id the change applies to.
    pub session: String,
}

fn item(info: gait_ops::mode::ModeInfo) -> ModeItem {
    ModeItem {
        name: info.name,
        allow: info.allow,
        deny: info.deny,
        steer: info.steer,
        description: info.description,
    }
}

/// List the named modes with their patterns.
pub async fn list(
    ctx: &GaitContext,
    _params: ModeListParams,
    _opts: CallOptions,
) -> anyhow::Result<Vec<ModeItem>> {
    let dir = ctx.gait_dir()?;
    gait_ops::mode::list(&dir)
        .iter()
        .map(|name| gait_ops::mode::show(&dir, name).map(item).map_err(infra))
        .collect()
}

/// Show one mode.
pub async fn show(
    ctx: &GaitContext,
    params: ModeRef,
    _opts: CallOptions,
) -> anyhow::Result<ModeItem> {
    let info = gait_ops::mode::show(&ctx.gait_dir()?, params.id.as_raw()).map_err(infra)?;
    Ok(item(info))
}

/// Write a new mode from allow and deny patterns, steering text and a description.
pub async fn add(
    ctx: &GaitContext,
    params: ModeAddParams,
    _opts: CallOptions,
) -> anyhow::Result<ModeAdded> {
    let info = gait_ops::mode::add(
        &ctx.gait_dir()?,
        params.id.as_raw(),
        &params.allow,
        &params.deny,
        params.steer.as_deref().unwrap_or(""),
        params.description.as_deref().unwrap_or(""),
    )
    .map_err(infra)?;
    Ok(ModeAdded {
        name: info.name,
        allow: info.allow,
        deny: info.deny,
        action: "added".into(),
    })
}

fn apply(ctx: &GaitContext, session: &str, name: &str) -> anyhow::Result<ModeSelected> {
    let dir = ctx.gait_dir()?;
    require_active(&dir, session)?;
    let selection = gait_ops::mode::resolve_selection(&dir, name).map_err(infra)?;
    gait_core::embed::record_session_update(
        &gait_core::git::Repos::production(),
        &dir,
        session,
        vec![selection.event_body],
    )
    .map_err(infra)?;
    Ok(ModeSelected {
        session: session.to_owned(),
        name: selection.info.name,
    })
}

/// Use a mode for a session from its next turn.
pub async fn select(
    ctx: &GaitContext,
    params: ModeSelectParams,
    _opts: CallOptions,
) -> anyhow::Result<ModeSelected> {
    apply(ctx, &params.session, params.id.as_raw())
}

/// Put a session back on the default mode.
pub async fn clear(
    ctx: &GaitContext,
    params: ModeClearParams,
    _opts: CallOptions,
) -> anyhow::Result<ModeSelected> {
    apply(ctx, &params.session, "default")
}
