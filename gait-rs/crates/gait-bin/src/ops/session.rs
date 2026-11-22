//! Session ops: list and inspect sessions, undo, persist or discard them, and
//! answer what a session is waiting for. Thin over `gait_ops`; the REPL and ACP
//! keep calling the same functions.
use super::GaitContext;
use clap::ValueEnum;
use gait_ops::permission::GrantScope;
use gait_ops::session::{BulkPlan, BulkVerb};
use op_registry::{CallOptions, OpParams, Ref, UsageError};
use std::path::Path;

output! {
    /// One active session.
    pub struct SessionItem {
        /// The session id.
        pub id: String,
        /// Wall-clock time of the first event, in Unix milliseconds; absent for a session with no log yet.
        pub created_at_unix_ms: Option<u64>,
        /// Wall-clock time of the last event, in Unix milliseconds; absent for a session with no log yet.
        pub updated_at_unix_ms: Option<u64>,
    }
}

output! {
    /// A mounted session and what it is doing.
    pub struct ProcessItem {
        /// The session id.
        pub session_id: String,
        /// One of `idle`, `running`, `awaiting-permission`, `awaiting-clarify`, `awaiting-inform`, `awaiting-unlock`.
        pub state: String,
    }
}

output! {
    /// Token use of a session, folded from its log.
    pub struct SessionStats {
        pub input: u64,
        pub output: u64,
        pub cache_read: u64,
        pub cache_write: u64,
        pub compaction_input: u64,
        pub compaction_output: u64,
        pub compaction_cache_read: u64,
        pub compaction_cache_write: u64,
        pub total: u64,
    }
}

output! {
    /// How often a tool ran.
    pub struct ToolCount {
        pub name: String,
        pub count: u32,
    }
}

output! {
    /// The last completed run of a session.
    pub struct SessionSummary {
        /// 1-indexed, as in `undo --list`; absent when no run has completed.
        pub turn: Option<u32>,
        pub message: String,
        /// Short checkpoint prefix; absent when the turn had no checkpoint.
        pub checkpoint_short: Option<String>,
        pub file_count: u32,
        pub added_lines: Option<u64>,
        pub removed_lines: Option<u64>,
        pub tools: Vec<ToolCount>,
    }
}

output! {
    /// One turn of a session that `undo` can go back to.
    pub struct UndoTurn {
        pub turn: u32,
        pub summary: String,
        /// Short checkpoint prefix; absent when the turn had no checkpoint.
        pub checkpoint_short: Option<String>,
    }
}

output! {
    /// What `undo` listed or did.
    pub struct UndoResult {
        /// The turns, when `--list` was given.
        pub turns: Vec<UndoTurn>,
        pub transcript_undone: bool,
        pub code_undone: bool,
        /// Short checkpoint prefix that was used; absent when there was none.
        pub checkpoint_short: Option<String>,
    }
}

output! {
    /// A session that was left alone, and why.
    pub struct SkippedSession {
        pub session_id: String,
        pub state: String,
    }
}

output! {
    /// A session that could not be acted on.
    pub struct FailedSession {
        pub session_id: String,
        pub error: String,
    }
}

output! {
    /// A non-fatal problem while acting on a session.
    pub struct SessionWarning {
        pub session_id: String,
        pub message: String,
    }
}

output! {
    /// What `persist` or `discard` did.
    pub struct SessionAct {
        /// `persisted`, `discarded`, or `dry-run` when nothing was changed.
        pub action: String,
        /// The sessions the act covers.
        pub eligible: Vec<String>,
        /// The sessions it was carried out on.
        pub acted: Vec<String>,
        pub skipped: Vec<SkippedSession>,
        pub failed: Vec<FailedSession>,
        pub warnings: Vec<SessionWarning>,
    }
}

output! {
    /// The pending permission requests that `allow` approved.
    pub struct AllowResult {
        pub session_id: String,
        pub scope: String,
        pub approved_count: usize,
    }
}

output! {
    /// The pending permission requests that `deny` refused.
    pub struct DenyResult {
        pub session_id: String,
        pub denied_count: usize,
    }
}

output! {
    /// The pending unlock requests that `unlock` approved.
    pub struct UnlockResult {
        pub session_id: String,
        pub scope: String,
        pub unlocked_count: usize,
        /// Grants written to the global config or the agent file.
        pub persisted_grants: usize,
        pub warnings: Vec<String>,
    }
}

output! {
    /// The pending question that `reply` answered.
    pub struct ReplyResult {
        pub session_id: String,
        /// `clarify` or `inform`.
        pub kind: String,
        /// The id of the answered item.
        pub id: String,
    }
}

/// The session every one of these ops acts on.
#[derive(Debug, Clone, OpParams)]
pub struct SessionId {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.
    #[op(subject = "session", id_only)]
    pub id: Ref,
}

#[derive(Debug, Clone, OpParams)]
pub struct SessionsParams {
    /// Show only the N most recently updated sessions.
    pub last: Option<usize>,
}

#[derive(Debug, Clone, OpParams)]
pub struct PsParams {}

#[derive(Debug, Clone, OpParams)]
pub struct UndoParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.
    #[op(subject = "session", id_only)]
    pub id: Ref,
    /// The turn to go back to, as numbered by `--list`. Turn 0 is the state before the first request and restores code only.
    #[op(conflicts_with = "list")]
    pub turn: Option<u32>,
    /// Undo the code only.
    pub code: bool,
    /// Undo the transcript only.
    pub transcript: bool,
    /// List the turns and their checkpoints instead of undoing.
    pub list: bool,
}

fn check_bulk(id: &Option<Ref>, all: bool, yes: bool, dry_run: bool) -> Result<(), UsageError> {
    if id.is_some() == all {
        return Err(UsageError::OneOf {
            group: "id or all".into(),
        });
    }
    if all && !yes && !dry_run {
        return Err(UsageError::InvalidField {
            name: "all".into(),
            reason: "--all needs --yes unless --dry-run".into(),
        });
    }
    Ok(())
}

/// The target and acknowledgement for `persist`.
#[derive(Debug, Clone, OpParams)]
#[op(validate = validate_persist)]
pub struct PersistParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.
    #[op(subject = "session", id_only)]
    pub id: Option<Ref>,
    /// Widen the set: for `processes list`, also finished processes; for `persist` and `discard`, every idle session that is not running (needs `--yes`).
    pub all: bool,
    /// Confirm acting on all sessions.
    pub yes: bool,
    /// Show what would be acted on and change nothing.
    pub dry_run: bool,
}

fn validate_persist(p: &PersistParams) -> Result<(), UsageError> {
    check_bulk(&p.id, p.all, p.yes, p.dry_run)
}

/// The target and acknowledgement for `discard`.
#[derive(Debug, Clone, OpParams)]
#[op(validate = validate_discard)]
pub struct DiscardParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.
    #[op(subject = "session", id_only)]
    pub id: Option<Ref>,
    /// Widen the set: for `processes list`, also finished processes; for `persist` and `discard`, every idle session that is not running (needs `--yes`).
    pub all: bool,
    /// Confirm acting on all sessions.
    pub yes: bool,
    /// Show what would be acted on and change nothing.
    pub dry_run: bool,
    /// Override a safety check: `init` may create the gait directory inside a codebase's git repository (a turn there then needs `--allow-intree`), `discard --all` may discard more than one session, `manual install` replaces an existing manual.
    pub force: bool,
}

fn validate_discard(p: &DiscardParams) -> Result<(), UsageError> {
    check_bulk(&p.id, p.all, p.yes, p.dry_run)
}

/// Where an allowed pattern is remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AllowScope {
    /// For this turn only (the default).
    Once,
    /// For the life of the session.
    Session,
    /// Saved to gait.jsonc.
    Global,
    /// Saved to the active mode's MODE.md.
    Mode,
}

#[derive(Debug, Clone, OpParams)]
pub struct AllowParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.
    #[op(subject = "session", id_only)]
    pub id: Ref,
    /// The tool pattern to approve; without it the pending requests are approved as asked.
    pub pattern: Option<String>,
    /// Where the change applies. When not given: `once` for `allow` and `unlock`, `global` for `register` and `unregister`.
    #[op(value_enum)]
    pub scope: Option<AllowScope>,
}

/// Where an unlock is remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UnlockScope {
    /// For this turn only, logged but not saved (the default).
    Once,
    /// For the life of the session, logged but not saved.
    Session,
    /// Saved to gait.jsonc.
    Global,
    /// Saved to the agent file named by `--agent`.
    Agent,
}

#[derive(Debug, Clone, OpParams)]
pub struct UnlockParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.
    #[op(subject = "session", id_only)]
    pub id: Ref,
    /// Where the change applies. When not given: `once` for `allow` and `unlock`, `global` for `register` and `unregister`.
    #[op(value_enum)]
    pub scope: Option<UnlockScope>,
    /// The agent config to change, for `--scope agent`: its name for `unlock`, the path of its `.md` file for `register` and `unregister`.
    pub agent: Option<String>,
}

#[derive(Debug, Clone, OpParams)]
pub struct ReplyParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.
    #[op(subject = "session", id_only)]
    pub id: Ref,
    /// The answer to the session's single pending question.
    #[op(allow_hyphen)]
    pub message: String,
}

fn dir(ctx: &GaitContext) -> anyhow::Result<std::path::PathBuf> {
    ctx.gait_dir()
}

/// List the active sessions, most recently updated first.
pub async fn sessions(
    ctx: &GaitContext,
    params: SessionsParams,
    _opts: CallOptions,
) -> anyhow::Result<Vec<SessionItem>> {
    let out = gait_ops::session::list(&dir(ctx)?, params.last)?;
    Ok(out
        .active
        .into_iter()
        .map(|row| SessionItem {
            id: row.session_id,
            created_at_unix_ms: row.created_at,
            updated_at_unix_ms: row.updated_at,
        })
        .collect())
}

/// Show each mounted session and its live state.
pub async fn ps(
    ctx: &GaitContext,
    _params: PsParams,
    _opts: CallOptions,
) -> anyhow::Result<Vec<ProcessItem>> {
    let out = gait_ops::session::ps(&dir(ctx)?)?;
    Ok(out
        .rows
        .into_iter()
        .map(|row| ProcessItem {
            session_id: row.session_id,
            state: row.state,
        })
        .collect())
}

/// Show a session's token use, split into agent work and compaction.
pub async fn stats(
    ctx: &GaitContext,
    params: SessionId,
    _opts: CallOptions,
) -> anyhow::Result<SessionStats> {
    let s = gait_ops::session::stats(&dir(ctx)?, params.id.as_raw())?;
    Ok(SessionStats {
        input: s.input,
        output: s.output,
        cache_read: s.cache_read,
        cache_write: s.cache_write,
        compaction_input: s.compaction_input,
        compaction_output: s.compaction_output,
        compaction_cache_read: s.compaction_cache_read,
        compaction_cache_write: s.compaction_cache_write,
        total: s.total,
    })
}

/// Show the outcome of a session's last completed run: its message, checkpoint, and the files and tools it touched.
pub async fn log(
    ctx: &GaitContext,
    params: SessionId,
    _opts: CallOptions,
) -> anyhow::Result<SessionSummary> {
    let s = gait_ops::session::summary(&dir(ctx)?, params.id.as_raw())?;
    Ok(SessionSummary {
        turn: s.turn,
        message: s.message,
        checkpoint_short: s.checkpoint_short,
        file_count: s.file_count,
        added_lines: s.added_lines,
        removed_lines: s.removed_lines,
        tools: s
            .tools
            .into_iter()
            .map(|t| ToolCount {
                name: t.name,
                count: t.count,
            })
            .collect(),
    })
}

/// Go back to an earlier turn of a session, or list the turns with `--list`.
pub async fn undo(
    ctx: &GaitContext,
    params: UndoParams,
    _opts: CallOptions,
) -> anyhow::Result<UndoResult> {
    let out = gait_ops::session::undo(gait_ops::session::UndoParams {
        gait_dir: dir(ctx)?,
        session_id: params.id.as_raw().to_owned(),
        turn: params.turn,
        code: params.code,
        transcript: params.transcript,
        list: params.list,
    })?;
    Ok(UndoResult {
        turns: out
            .turns
            .into_iter()
            .map(|t| UndoTurn {
                turn: t.turn,
                summary: t.summary,
                checkpoint_short: t.checkpoint_short,
            })
            .collect(),
        transcript_undone: out.transcript_undone,
        code_undone: out.code_undone,
        checkpoint_short: out.checkpoint_short,
    })
}

fn act(
    gait_dir: &Path,
    verb: BulkVerb,
    id: Option<Ref>,
    dry_run: bool,
    force: bool,
) -> anyhow::Result<SessionAct> {
    use gait_ops::session::{bulk_act, classify_bulk, require_session};
    let done = match verb {
        BulkVerb::Persist => "persisted",
        BulkVerb::Discard => "discarded",
    };
    let plan: BulkPlan = match &id {
        Some(id) => {
            require_session(gait_dir, id.as_raw())?;
            let plan = classify_bulk(gait_dir, verb)?;
            if !plan.eligible.iter().any(|s| s == id.as_raw()) {
                let why = plan
                    .skipped
                    .iter()
                    .find(|(s, _)| s == id.as_raw())
                    .map(|(_, state)| format!(" ({state})"))
                    .unwrap_or_default();
                return Err(gait_ops::OpError::invalid(format!(
                    "session {} is not eligible to {}{why}",
                    id.as_raw(),
                    if verb == BulkVerb::Persist {
                        "persist"
                    } else {
                        "discard"
                    }
                )));
            }
            BulkPlan {
                verb,
                eligible: vec![id.as_raw().to_owned()],
                skipped: Vec::new(),
            }
        }
        None => classify_bulk(gait_dir, verb)?,
    };
    let skipped = |plan: &[(String, String)]| {
        plan.iter()
            .map(|(id, state)| SkippedSession {
                session_id: id.clone(),
                state: state.clone(),
            })
            .collect::<Vec<_>>()
    };
    if dry_run || plan.eligible.is_empty() {
        let action = if dry_run { "dry-run" } else { done };
        return Ok(SessionAct {
            action: action.into(),
            skipped: skipped(&plan.skipped),
            eligible: plan.eligible,
            acted: Vec::new(),
            failed: Vec::new(),
            warnings: Vec::new(),
        });
    }
    // A single session is its own acknowledgement; for many, `force` is the
    // second opt-in the discard gate asks for.
    let force = id.is_some() || force || verb == BulkVerb::Persist;
    let outcome = bulk_act(gait_dir, &plan, true, force)?;
    Ok(SessionAct {
        action: done.into(),
        eligible: outcome.eligible,
        acted: outcome.acted,
        skipped: skipped(&outcome.skipped),
        failed: outcome
            .failed
            .into_iter()
            .map(|(id, error)| FailedSession {
                session_id: id,
                error,
            })
            .collect(),
        warnings: outcome
            .warnings
            .into_iter()
            .map(|(id, message)| SessionWarning {
                session_id: id,
                message,
            })
            .collect(),
    })
}

/// Merge a session onto main and remove its worktree, or all idle sessions with `--all --yes`.
pub async fn persist(
    ctx: &GaitContext,
    params: PersistParams,
    _opts: CallOptions,
) -> anyhow::Result<SessionAct> {
    act(
        &dir(ctx)?,
        BulkVerb::Persist,
        params.id,
        params.dry_run,
        false,
    )
}

/// Remove a session's worktree without merging; unrecoverable. `--all --yes --force` does it for all idle sessions.
pub async fn discard(
    ctx: &GaitContext,
    params: DiscardParams,
    _opts: CallOptions,
) -> anyhow::Result<SessionAct> {
    act(
        &dir(ctx)?,
        BulkVerb::Discard,
        params.id,
        params.dry_run,
        params.force,
    )
}

/// Approve the permission requests a session is waiting on.
pub async fn allow(
    ctx: &GaitContext,
    params: AllowParams,
    _opts: CallOptions,
) -> anyhow::Result<AllowResult> {
    let (scope, name) = match params.scope.unwrap_or(AllowScope::Once) {
        AllowScope::Once => (GrantScope::Once, "once"),
        AllowScope::Session => (GrantScope::Session, "session"),
        AllowScope::Global => (GrantScope::Global, "global"),
        AllowScope::Mode => (GrantScope::Mode, "mode"),
    };
    let out = gait_ops::permission::allow(
        &dir(ctx)?,
        params.id.as_raw(),
        gait_ops::permission::AllowParams {
            scope,
            pattern: params.pattern,
        },
    )?;
    Ok(AllowResult {
        session_id: params.id.as_raw().to_owned(),
        scope: name.into(),
        approved_count: out.reply_items.len(),
    })
}

/// Refuse the permission requests a session is waiting on.
pub async fn deny(
    ctx: &GaitContext,
    params: SessionId,
    _opts: CallOptions,
) -> anyhow::Result<DenyResult> {
    let out = gait_ops::permission::deny(&dir(ctx)?, params.id.as_raw())?;
    Ok(DenyResult {
        session_id: params.id.as_raw().to_owned(),
        denied_count: out.reply_items.len(),
    })
}

/// Approve the sandbox unlock requests a session is waiting on.
pub async fn unlock(
    ctx: &GaitContext,
    params: UnlockParams,
    _opts: CallOptions,
) -> anyhow::Result<UnlockResult> {
    let (scope, name) = match params.scope.unwrap_or(UnlockScope::Once) {
        UnlockScope::Once => (GrantScope::Once, "once"),
        UnlockScope::Session => (GrantScope::Session, "session"),
        UnlockScope::Global => (GrantScope::Global, "global"),
        UnlockScope::Agent => (GrantScope::Agent, "agent"),
    };
    let out = gait_ops::permission::unlock(gait_ops::permission::UnlockParams {
        gait_dir: dir(ctx)?,
        session_id: params.id.as_raw().to_owned(),
        scope,
        agent: params.agent,
    })?;
    Ok(UnlockResult {
        session_id: params.id.as_raw().to_owned(),
        scope: name.into(),
        unlocked_count: out.reply_items.len(),
        persisted_grants: out.persisted_grants,
        warnings: out.warnings,
    })
}

/// Answer the single clarify or inform question a session is waiting on.
pub async fn reply(
    ctx: &GaitContext,
    params: ReplyParams,
    _opts: CallOptions,
) -> anyhow::Result<ReplyResult> {
    use gait_ops::interaction::ReplyKind;
    let out = gait_ops::interaction::reply(&dir(ctx)?, params.id.as_raw(), params.message)?;
    Ok(ReplyResult {
        session_id: params.id.as_raw().to_owned(),
        kind: match out.kind {
            ReplyKind::Clarify => "clarify",
            ReplyKind::Inform => "inform",
        }
        .into(),
        id: out.id,
    })
}
