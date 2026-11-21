//! Inference ops: the named model profiles of a gait directory, and choosing
//! one for a session. Thin over `gait_ops::inference`.
use super::{infra, require_active, GaitContext};
use op_registry::{CallOptions, OpParams, Ref};

output! {
    /// A named inference profile.
    pub struct InferenceItem {
        /// The profile name.
        pub name: String,
        /// The model, in `provider/id` form.
        pub model: String,
    }
}

output! {
    /// An inference profile that was written.
    pub struct InferenceAdded {
        pub name: String,
        pub model: String,
        /// Always `added`.
        pub action: String,
    }
}

output! {
    /// The inference a session now uses.
    pub struct InferenceSelected {
        /// The session id.
        pub session: String,
        /// The profile name, `default` after `clear`.
        pub name: String,
        /// The model; empty for the default.
        pub model: String,
    }
}

#[derive(Debug, Clone, OpParams)]
pub struct InferenceListParams {}

/// An inference profile that must exist.
#[derive(Debug, Clone, OpParams)]
pub struct InferenceRef {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or daemon.
    #[op(subject = "inference")]
    pub id: Ref,
}

#[derive(Debug, Clone, OpParams)]
pub struct InferenceAddParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or daemon.
    #[op(subject = "inference", id_only, absent_ok)]
    pub id: Ref,
    /// The model, in `provider/id` form, for example `opencode/kimi-k2.6`.
    pub model: String,
    /// Begin compacting the conversation when it reaches this many tokens.
    pub compaction_threshold_tokens: u64,
}

#[derive(Debug, Clone, OpParams)]
pub struct InferenceSelectParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or daemon.
    #[op(subject = "inference")]
    pub id: Ref,
    /// The session id the change applies to.
    pub session: String,
}

#[derive(Debug, Clone, OpParams)]
pub struct InferenceClearParams {
    /// The session id the change applies to.
    pub session: String,
}

fn item(info: gait_ops::inference::InferenceInfo) -> InferenceItem {
    InferenceItem {
        name: info.name,
        model: info.model,
    }
}

/// List the named inference profiles with their models.
pub async fn list(
    ctx: &GaitContext,
    _params: InferenceListParams,
    _opts: CallOptions,
) -> anyhow::Result<Vec<InferenceItem>> {
    let dir = ctx.gait_dir()?;
    gait_ops::inference::list(&dir)
        .iter()
        .map(|name| {
            gait_ops::inference::show(&dir, name)
                .map(item)
                .map_err(infra)
        })
        .collect()
}

/// Show one inference profile.
pub async fn show(
    ctx: &GaitContext,
    params: InferenceRef,
    _opts: CallOptions,
) -> anyhow::Result<InferenceItem> {
    let info = gait_ops::inference::show(&ctx.gait_dir()?, params.id.as_raw()).map_err(infra)?;
    Ok(item(info))
}

/// Write a new inference profile: a model and the token count at which compaction begins.
pub async fn add(
    ctx: &GaitContext,
    params: InferenceAddParams,
    _opts: CallOptions,
) -> anyhow::Result<InferenceAdded> {
    let info = gait_ops::inference::add(
        &ctx.gait_dir()?,
        params.id.as_raw(),
        &params.model,
        params.compaction_threshold_tokens,
    )
    .map_err(infra)?;
    Ok(InferenceAdded {
        name: info.name,
        model: info.model,
        action: "added".into(),
    })
}

fn apply(ctx: &GaitContext, session: &str, name: &str) -> anyhow::Result<InferenceSelected> {
    let dir = ctx.gait_dir()?;
    require_active(&dir, session)?;
    let selection = gait_ops::inference::resolve_selection(&dir, name).map_err(infra)?;
    gait_core::embed::record_session_update(
        &gait_core::git::Repos::production(),
        &dir,
        session,
        vec![selection.event_body],
    )
    .map_err(infra)?;
    Ok(InferenceSelected {
        session: session.to_owned(),
        name: selection.info.name,
        model: selection.info.model,
    })
}

/// Use an inference profile for a session from its next turn.
pub async fn select(
    ctx: &GaitContext,
    params: InferenceSelectParams,
    _opts: CallOptions,
) -> anyhow::Result<InferenceSelected> {
    apply(ctx, &params.session, params.id.as_raw())
}

/// Put a session back on the default inference.
pub async fn clear(
    ctx: &GaitContext,
    params: InferenceClearParams,
    _opts: CallOptions,
) -> anyhow::Result<InferenceSelected> {
    apply(ctx, &params.session, "default")
}
