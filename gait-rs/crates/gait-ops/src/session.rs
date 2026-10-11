//! Session-admin verbs: `list`, `ps`, `stats`, `undo`, `classify_bulk`, `bulk_act`.
//!
//! Sources: `crates/gait-cli/src/cli/session.rs`. Read-only ops wrap
//! `gait_state::git::Repos`, `gait_state::session::log`, and
//! `gait_state::session::retrace`. Mutating ops take a `SessionLock`; bulk
//! persist/discard uses the preview→act pattern (design §4).
use crate::OpError;
use std::path::{Path, PathBuf};

// -----------------------------------------------------------------------------
// list  (cmd_sessions)
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SessionRow {
    pub session_id: String,
    /// First event's wall-clock ms, or `None` for a session with no log yet.
    pub created_at: Option<u64>,
    /// Last event's wall-clock ms, or `None` for a session with no log yet.
    pub updated_at: Option<u64>,
}

#[derive(Debug, serde::Serialize)]
pub struct ListOutcome {
    /// Active (unmerged) sessions, most-recently-updated first.
    pub active: Vec<SessionRow>,
}

impl ListOutcome {
    /// The CLI's only rendering of `sessions` — no plain-text form exists.
    pub fn to_json(&self, pretty: bool) -> String {
        crate::render_json(self, pretty)
    }
}

/// Walk `gait_dir`'s `session/*` branches and return the ones main is not a
/// descendant of, each enriched with its log's first/last event timestamps,
/// sorted most-recently-updated first. Read-only; no log/branch writes.
///
/// `last`, if set, truncates to that many most-recently-updated sessions
/// (`gait sessions --last <n>`).
pub fn list(gait_dir: &Path, last: Option<usize>) -> Result<ListOutcome, OpError> {
    let ids = gait_state::git::Repos::production()
        .traces
        .list_active_sessions(gait_dir)?;
    let mut rows: Vec<SessionRow> = ids
        .into_iter()
        .map(|session_id| {
            let tx_path = gait_state::session::live::log_path(gait_dir, &session_id);
            let events = gait_state::session::log::read_events(&tx_path).unwrap_or_default();
            SessionRow {
                created_at: events.first().map(|e| e.ts),
                updated_at: events.last().map(|e| e.ts),
                session_id,
            }
        })
        .collect();
    // Most-recently-updated first; a session with no log yet (None) sorts last.
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    if let Some(n) = last {
        rows.truncate(n);
    }
    Ok(ListOutcome { active: rows })
}

// -----------------------------------------------------------------------------
// ps  (cmd_ps)
// -----------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
pub struct PsRow {
    pub session_id: String,
    pub state: String,
}

#[derive(Debug)]
pub struct PsOutcome {
    pub rows: Vec<PsRow>,
}

impl PsOutcome {
    /// `(session_id, state)` rows, in the order returned by `ps` — kept for
    /// `gait-repl`'s own (separate, non-JSON) rendering; `gait-cli` no longer
    /// calls this, it always renders `to_json`.
    pub fn to_rows(&self) -> Vec<(String, String)> {
        self.rows
            .iter()
            .map(|r| (r.session_id.clone(), r.state.clone()))
            .collect()
    }

    /// The CLI's only rendering: `[{"session_id": .., "state": ..}, ...]`,
    /// compact or pretty-printed per `pretty`. This is the API contract — the
    /// CLI and any future frontend render this exact string rather than
    /// re-deriving the shape.
    pub fn to_json(&self, pretty: bool) -> String {
        crate::render_json(&self.rows, pretty)
    }
}

/// Walk `.active/<id>/...`, classify each as idle / awaiting-*. Read-only.
pub fn ps(gait_dir: &Path) -> Result<PsOutcome, OpError> {
    use gait_state::session::log::{session_state, SessionState};
    let sessions_dir = gait_dir.join(".active");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(_) => return Ok(PsOutcome { rows: Vec::new() }),
    };
    let mut rows: Vec<PsRow> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let tx_path = gait_state::session::live::log_path(gait_dir, id);
        let state_str = match session_state(&tx_path) {
            // Idle in the log ≠ idle in reality: a session between turns still
            // holds its `.active/<id>/lock` while its process runs. Consult the
            // live lock so a running turn reports "running", not "idle" — the
            // same check `classify_bulk` applies to the Idle case.
            Ok(SessionState::Idle) => {
                if gait_state::session::SessionLock::is_running(&path) {
                    "running".to_string()
                } else {
                    "idle".to_string()
                }
            }
            Ok(SessionState::AwaitingPermission { .. }) => "awaiting-permission".to_string(),
            Ok(SessionState::AwaitingClarify { .. }) => "awaiting-clarify".to_string(),
            Ok(SessionState::AwaitingInform { .. }) => "awaiting-inform".to_string(),
            Ok(SessionState::AwaitingSandboxUnlock { .. }) => "awaiting-unlock".to_string(),
            Err(_) => continue, // unparseable / non-session dir
        };
        rows.push(PsRow {
            session_id: id.to_string(),
            state: state_str,
        });
    }
    Ok(PsOutcome { rows })
}

// -----------------------------------------------------------------------------
// stats  (cmd_stats)
// -----------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
pub struct StatsOutcome {
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

impl StatsOutcome {
    /// The CLI's only rendering: distinguishes harness compaction from normal
    /// agent work while retaining one `total` for cost accounting.
    pub fn to_json(&self, pretty: bool) -> String {
        crate::render_json(self, pretty)
    }
}

/// Cumulative token usage for a session, folded from the log.
pub fn stats(gait_dir: &Path, session_id: &str) -> Result<StatsOutcome, OpError> {
    use gait_state::session::log::read_events;
    use gait_state::session::retrace::total_usage;
    let tx_path = gait_state::session::live::log_path(gait_dir, session_id);
    let events = read_events(&tx_path).map_err(anyhow::Error::from)?;
    let u = total_usage(&events);
    let (compaction_input, compaction_output, compaction_cache_read, compaction_cache_write) =
        events
            .iter()
            .fold((0u64, 0u64, 0u64, 0u64), |total, event| {
                let Some(gait_state::proto::gait::log::v1::event::Body::Payload(payload)) =
                    &event.body
                else {
                    return total;
                };
                let Some(gait_state::proto::gait::log::v1::payload::Kind::Summary(summary)) =
                    &payload.kind
                else {
                    return total;
                };
                let Some(usage) = &summary.usage else {
                    return total;
                };
                (
                    total.0.saturating_add(usage.input),
                    total.1.saturating_add(usage.output),
                    total.2.saturating_add(usage.cache_read),
                    total.3.saturating_add(usage.cache_write),
                )
            });
    let total = u.input
        + u.output
        + u.cache_read
        + u.cache_write
        + compaction_input
        + compaction_output
        + compaction_cache_read
        + compaction_cache_write;
    Ok(StatsOutcome {
        input: u.input,
        output: u.output,
        cache_read: u.cache_read,
        cache_write: u.cache_write,
        compaction_input,
        compaction_output,
        compaction_cache_read,
        compaction_cache_write,
        total,
    })
}

/// The message of the `TurnDone` immediately preceding a `RunComplete` at
/// `seq`/`run` — the "what did the last turn of this run say" lookup shared
/// by `undo`'s per-turn list and `summary`'s last-run-only view. Empty
/// string if the turn ended with no message (e.g. an empty `TurnDone`).
fn turn_done_message_before(
    events: &[gait_state::proto::gait::log::v1::Event],
    seq: u64,
    run: Option<u32>,
) -> String {
    use gait_state::proto::gait::log::v1::{event, payload};
    events
        .iter()
        .rev()
        .filter(|candidate| candidate.seq < seq && candidate.run == run)
        .find_map(|candidate| match &candidate.body {
            Some(event::Body::Payload(payload)) => match payload.kind.as_ref() {
                Some(payload::Kind::TurnResult(turn)) => match turn.outcome.as_ref() {
                    Some(gait_state::proto::gait::log::v1::turn_result::Outcome::Done(done)) => {
                        match done.output.as_ref() {
                            Some(gait_state::proto::gait::log::v1::turn_done::Output::Text(
                                text,
                            )) => Some(text.message.clone()),
                            Some(
                                gait_state::proto::gait::log::v1::turn_done::Output::Structured(
                                    output,
                                ),
                            ) => Some(output.message.clone()),
                            None => Some(String::new()),
                        }
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_default()
}

// -----------------------------------------------------------------------------
// undo  (cmd_undo)
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct UndoTurn {
    pub turn: u32,
    pub summary: String,
    /// Short (8-char) checkpoint prefix, or `None` if the turn had no checkpoint.
    pub checkpoint_short: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct UndoParams {
    pub gait_dir: PathBuf,
    pub session_id: String,
    pub turn: Option<u32>,
    pub code: bool,
    pub transcript: bool,
    pub list: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct UndoOutcome {
    /// Populated when `list: true` (turn list with checkpoint prefixes).
    pub turns: Vec<UndoTurn>,
    /// True if a transcript git-walk actually moved the worktree.
    pub transcript_undone: bool,
    /// True if a checkpoint `restore_to` actually moved the codebase.
    pub code_undone: bool,
    /// Short checkpoint prefix used (8 chars), or `None` if no checkpoint.
    pub checkpoint_short: Option<String>,
}

impl UndoOutcome {
    /// The CLI's only rendering, covering both `list` mode (`turns`
    /// populated) and act mode (`code_undone`/`transcript_undone`/
    /// `checkpoint_short` populated) — one shape, fields simply empty/false/
    /// null in whichever mode didn't apply.
    pub fn to_json(&self, pretty: bool) -> String {
        crate::render_json(self, pretty)
    }
}

/// Port of `cmd_undo`'s two modes. `list: true` returns the turn list (no
/// mutation). Otherwise `turn` is required; default scope (no `code`/`transcript`
/// flag) acts on both axes. The op takes the `SessionLock` itself.
///
/// "Turn N" is the Nth completed *run* (one user request): its code target is
/// that run's final checkpoint and its transcript target is the newest session
/// commit of that run. `turn 0` is the state before the first request and
/// restores code only (see `undo_with`).
pub fn undo(params: UndoParams) -> Result<UndoOutcome, OpError> {
    undo_with(params, gait_state::git::Repos::production().checkpoints.as_ref())
}

fn undo_with(
    params: UndoParams,
    checkpoints: &dyn gait_state::git::checkpoint::CheckpointRepo,
) -> Result<UndoOutcome, OpError> {
    use gait_state::proto::gait::log::v1::{event, payload};
    use gait_state::session::log::read_events;

    if params.list && params.turn.is_some() {
        return Err(OpError::Conflict(
            "--list and --turn are mutually exclusive".to_string(),
        ));
    }

    let tx_path = gait_state::session::live::log_path(&params.gait_dir, &params.session_id);
    let events = read_events(&tx_path).map_err(anyhow::Error::from)?;

    // Join each completed run's terminal checkpoint with its final TurnResult.
    // `turns[i]` is user-facing turn i+1; `run_ids[i]` is that run's ordinal
    // (a run that was cancelled, failed or paused has no RunComplete, so the
    // turn number and the run ordinal can differ).
    let mut turns: Vec<UndoTurn> = Vec::new();
    let mut full_shas: Vec<Option<String>> = Vec::new();
    let mut run_ids: Vec<u32> = Vec::new();
    // Baseline checkpoint of the earliest run: the state before the first request.
    let mut first_baseline: Option<(u32, Option<String>)> = None;
    for e in &events {
        if let Some(event::Body::Payload(p)) = &e.body {
            match &p.kind {
                Some(payload::Kind::RunStart(start)) => {
                    if first_baseline.as_ref().is_none_or(|(i, _)| start.index < *i) {
                        first_baseline = Some((
                            start.index,
                            start.baseline_checkpoint.clone().filter(|s| !s.is_empty()),
                        ));
                    }
                }
                Some(payload::Kind::RunComplete(complete)) => {
                    let run = e.run.ok_or_else(|| {
                        OpError::Conflict("RunComplete event has no run ordinal".to_string())
                    })?;
                    let idx = turns.len() as u32 + 1;
                    let message = turn_done_message_before(&events, e.seq, e.run);
                    let full = complete
                        .final_state
                        .as_ref()
                        .and_then(|state| state.checkpoint.clone());
                    let checkpoint_short =
                        full.as_deref().map(|s| s[..s.len().min(8)].to_string());
                    turns.push(UndoTurn {
                        turn: idx,
                        summary: message,
                        checkpoint_short,
                    });
                    full_shas.push(full);
                    run_ids.push(run);
                }
                _ => {}
            }
        }
    }

    if params.list {
        return Ok(UndoOutcome {
            turns,
            transcript_undone: false,
            code_undone: false,
            checkpoint_short: None,
        });
    }

    let turn = params
        .turn
        .ok_or_else(|| OpError::Conflict("--turn <n> is required (or use --list)".to_string()))?;

    let do_code = params.code || !params.transcript;
    let do_transcript = params.transcript || !params.code;

    // Resolve every target before touching anything, so a missing transcript
    // commit cannot leave the code restored and the transcript not.
    let (full_sha, checkpoint_short, transcript_target) = if turn == 0 {
        // Before the first request. The transcript cannot go back that far:
        // the session's init commit has an empty tree, so resetting to it
        // would delete log.jsonl, which the log writer cannot recreate.
        if do_transcript {
            return Err(OpError::Conflict(
                "turn 0 (before the first request) restores code only: use \
                 `gait undo <session> --turn 0 --code` (REPL: /uc 0)"
                    .to_string(),
            ));
        }
        let sha = first_baseline.and_then(|(_, sha)| sha).ok_or_else(|| {
            OpError::Conflict("no baseline checkpoint recorded before the first request".to_string())
        })?;
        let short = Some(sha[..sha.len().min(8)].to_string());
        (Some(sha), short, None)
    } else {
        let turn_idx = turn as usize - 1;
        if turn_idx >= turns.len() {
            return Err(OpError::NotFound(format!(
                "no turn {} in session {}",
                turn, params.session_id
            )));
        }
        let target = if do_transcript {
            let dir = gait_state::session::live::worktree_dir(&params.gait_dir, &params.session_id);
            Some(find_transcript_commit(&dir, &params.session_id, run_ids[turn_idx])?)
        } else {
            None
        };
        (
            full_shas[turn_idx].clone(),
            turns[turn_idx].checkpoint_short.clone(),
            target,
        )
    };

    let codebase = match (do_code, full_sha.as_deref()) {
        (true, Some(_)) => Some(
            gait_state::session::read_session_meta(&params.gait_dir, &params.session_id)?
                .codebase_path
                .map(std::path::PathBuf::from)
                .ok_or_else(|| {
                    OpError::Conflict("session has no codebase_path; cannot undo code".to_string())
                })?,
        ),
        _ => None,
    };

    // Acquire the lock; refuse if a turn is in progress.
    let session_dir = gait_state::session::live::worktree_dir(&params.gait_dir, &params.session_id);
    let _lock = gait_state::session::SessionLock::acquire(&session_dir).map_err(|e| {
        OpError::SessionBusy(format!("cannot undo: {} (is a turn in progress?)", e))
    })?;

    let mut code_undone = false;
    let mut transcript_undone = false;

    if let (Some(codebase), Some(sha)) = (codebase.as_deref(), full_sha.as_deref()) {
        checkpoints.restore_to(&params.gait_dir, &params.session_id, codebase, sha)?;
        code_undone = true;
    }

    if let Some(oid) = transcript_target {
        reset_transcript_to(&session_dir, oid)?;
        transcript_undone = true;
    }

    Ok(UndoOutcome {
        turns: Vec::new(),
        transcript_undone,
        code_undone,
        checkpoint_short,
    })
}

/// The newest session commit of run `run`: `session(<id>): run <run> turn <t>`.
/// A run spans one commit per turn process (a pause and resume add another),
/// and the newest holds the log as it stood when the run ended. The trailing
/// space keeps run 1 from matching run 10.
fn find_transcript_commit(dir: &Path, session_id: &str, run: u32) -> Result<git2::Oid, OpError> {
    let repo = git2::Repository::open(dir).map_err(anyhow::Error::from)?;
    let prefix = format!("session({session_id}): run {run} turn ");
    let mut walk = repo.revwalk().map_err(anyhow::Error::from)?;
    walk.push_head().map_err(anyhow::Error::from)?;
    for oid in walk {
        let oid = oid.map_err(anyhow::Error::from)?;
        let commit = repo.find_commit(oid).map_err(anyhow::Error::from)?;
        if commit.summary().is_some_and(|s| s.starts_with(&prefix)) {
            return Ok(oid);
        }
    }
    Err(OpError::NotFound(format!(
        "no transcript commit for run {run} in session {session_id}"
    )))
}

fn reset_transcript_to(dir: &Path, oid: git2::Oid) -> Result<(), OpError> {
    let repo = git2::Repository::open(dir).map_err(anyhow::Error::from)?;
    let obj = repo.find_object(oid, None).map_err(anyhow::Error::from)?;
    repo.reset(&obj, git2::ResetType::Hard, None)
        .map_err(anyhow::Error::from)?;
    Ok(())
}

// -----------------------------------------------------------------------------
// summary  (cmd_log)
// -----------------------------------------------------------------------------

#[derive(Debug, PartialEq, serde::Serialize)]
pub struct SummaryToolCount {
    pub name: String,
    pub count: u32,
}

#[derive(Debug, serde::Serialize)]
pub struct SessionSummaryOutcome {
    /// 1-indexed, same numbering as `undo --list`'s turns. `None` if the
    /// session has not completed a run yet.
    pub turn: Option<u32>,
    pub message: String,
    /// Short (8-char) checkpoint prefix, or `None` if the turn had no checkpoint.
    pub checkpoint_short: Option<String>,
    pub file_count: u32,
    pub added_lines: Option<u64>,
    pub removed_lines: Option<u64>,
    pub tools: Vec<SummaryToolCount>,
}

impl SessionSummaryOutcome {
    /// The CLI's only rendering of `log <session-id>` — no plain-text form exists.
    pub fn to_json(&self, pretty: bool) -> String {
        crate::render_json(self, pretty)
    }
}

/// The last completed run's outcome: its final turn's `TurnDone` message,
/// the checkpoint it landed on, and its `RunSummary` (files changed, tools
/// used) — without reading or returning the full event stream. `gait log
/// <session-id>` is meant as a quick "what happened" glance; `gait sessions`
/// (per-session log path) or a direct log read is the tool for the full
/// history.
pub fn summary(gait_dir: &Path, session_id: &str) -> Result<SessionSummaryOutcome, OpError> {
    use gait_state::proto::gait::log::v1::{event, payload};
    use gait_state::session::log::read_events;

    let tx_path = gait_state::session::live::log_path(gait_dir, session_id);
    let events = read_events(&tx_path).map_err(anyhow::Error::from)?;

    let mut turn_idx = 0u32;
    let mut last: Option<SessionSummaryOutcome> = None;
    for e in &events {
        let Some(event::Body::Payload(p)) = &e.body else {
            continue;
        };
        let Some(payload::Kind::RunComplete(complete)) = &p.kind else {
            continue;
        };
        turn_idx += 1;
        let message = turn_done_message_before(&events, e.seq, e.run);
        let checkpoint_short = complete
            .final_state
            .as_ref()
            .and_then(|s| s.checkpoint.as_deref())
            .map(|s| s[..s.len().min(8)].to_string());
        let (file_count, added_lines, removed_lines, tools) = match complete.summary.as_ref() {
            Some(summary) => {
                let changes = summary.changes.clone().unwrap_or_default();
                let tools = summary
                    .tools
                    .iter()
                    .map(|t| SummaryToolCount {
                        name: t.name.clone(),
                        count: t.count,
                    })
                    .collect();
                (
                    changes.file_count,
                    changes.added_lines,
                    changes.removed_lines,
                    tools,
                )
            }
            None => (0, None, None, Vec::new()),
        };
        last = Some(SessionSummaryOutcome {
            turn: Some(turn_idx),
            message,
            checkpoint_short,
            file_count,
            added_lines,
            removed_lines,
            tools,
        });
    }
    Ok(last.unwrap_or(SessionSummaryOutcome {
        turn: None,
        message: String::new(),
        checkpoint_short: None,
        file_count: 0,
        added_lines: None,
        removed_lines: None,
        tools: Vec::new(),
    }))
}

// -----------------------------------------------------------------------------
// bulk  (classify_bulk / bulk_act for persist + discard)
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkVerb {
    Persist,
    Discard,
}

impl BulkVerb {
    fn label(&self) -> &'static str {
        match self {
            BulkVerb::Persist => "persist",
            BulkVerb::Discard => "discard",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BulkPlan {
    pub verb: BulkVerb,
    /// Session ids that will be acted on.
    pub eligible: Vec<String>,
    /// Session ids that will be skipped, with the reason.
    pub skipped: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct BulkOutcome {
    pub verb: BulkVerb,
    /// The eligible set (snapshot of the plan).
    pub eligible: Vec<String>,
    /// The skipped set (snapshot of the plan).
    pub skipped: Vec<(String, String)>,
    /// Ids the act loop successfully completed.
    pub acted: Vec<String>,
    /// (id, error) pairs the act loop failed on.
    pub failed: Vec<(String, String)>,
    /// (id, message) non-fatal warnings from the act loop (e.g. a persisted
    /// session whose checkpoint db could not be removed). The adapter renders
    /// these; the op itself never prints.
    pub warnings: Vec<(String, String)>,
}

/// The shared `--json` shape for both a bulk *preview* (`BulkPlan`, before any
/// action ran — `acted`/`failed` are empty) and a bulk *outcome* (`BulkOutcome`,
/// after `bulk_act`). One source of truth so the CLI and any future frontend
/// (REPL, ACP) never re-derive this shape independently (design §5.2/§10).
fn render_bulk_json(
    eligible: &[String],
    skipped: &[(String, String)],
    acted: &[String],
    failed: &[(String, String)],
    pretty: bool,
) -> String {
    let v = serde_json::json!({
        "eligible": eligible,
        "skipped": skipped.iter()
            .map(|(id, s)| serde_json::json!({"session_id": id, "state": s}))
            .collect::<Vec<_>>(),
        "acted": acted,
        "failed": failed.iter()
            .map(|(id, e)| serde_json::json!({"session_id": id, "error": e}))
            .collect::<Vec<_>>(),
    });
    crate::render_json(&v, pretty)
}

impl BulkPlan {
    /// The CLI's only rendering of the preview (before `bulk_act` runs):
    /// `acted`/`failed` are empty arrays.
    pub fn to_json(&self, pretty: bool) -> String {
        render_bulk_json(&self.eligible, &self.skipped, &[], &[], pretty)
    }
}

impl BulkOutcome {
    /// The CLI's only rendering of the completed action.
    pub fn to_json(&self, pretty: bool) -> String {
        render_bulk_json(
            &self.eligible,
            &self.skipped,
            &self.acted,
            &self.failed,
            pretty,
        )
    }
}

/// PURE preview. Classifies targets, performs NO effect. The frontend renders
/// the plan and prompts using `eligible.len()`.
pub fn classify_bulk(gait_dir: &Path, verb: BulkVerb) -> Result<BulkPlan, OpError> {
    let (eligible, skipped) = classify_bulk_targets(gait_dir)?;
    Ok(BulkPlan {
        verb,
        eligible,
        skipped,
    })
}

/// Act on a previously-computed plan. `confirmed: false` returns
/// `OpError::ConfirmationRequired` and mutates nothing.
///
/// `force` is a second, distinct gate that only matters for a *bulk*
/// discard (`verb == Discard` with more than one eligible session):
/// discard is unrecoverable (no merge), so a single blanket `confirmed`
/// (which a non-interactive caller can set once and forget, e.g. a
/// scripted `--yes`) isn't enough acknowledgment for wiping out an
/// unbounded number of sessions in one call. A caller must set `force`
/// too, on top of `confirmed`, specifically to bulk-discard more than one
/// session. Single-session discards (`eligible.len() <= 1`, whether from
/// `discard <id>` or a `--all` plan that happens to match one session)
/// are unaffected — `force` is ignored for those.
pub fn bulk_act(
    gait_dir: &Path,
    plan: &BulkPlan,
    confirmed: bool,
    force: bool,
) -> Result<BulkOutcome, OpError> {
    if plan.verb == BulkVerb::Discard && plan.eligible.len() > 1 && !force {
        return Err(OpError::ConfirmationRequired(format!(
            "bulk discard of {} sessions requires --force (a second, distinct flag from \
             confirmation) — review the eligible list above, then re-run with --force",
            plan.eligible.len()
        )));
    }
    if !confirmed {
        return Err(OpError::ConfirmationRequired(format!(
            "{} {} session(s) requires confirmation",
            plan.verb.label(),
            plan.eligible.len()
        )));
    }

    let mut acted: Vec<String> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut warnings: Vec<(String, String)> = Vec::new();
    for id in &plan.eligible {
        let session_dir = gait_state::session::live::worktree_dir(gait_dir, id);
        let _lock = match gait_state::session::SessionLock::acquire(&session_dir) {
            Ok(l) => l,
            Err(_) => {
                failed.push((id.clone(), "started running".to_string()));
                continue;
            }
        };
        let result = match plan.verb {
            BulkVerb::Persist => persist_one(gait_dir, id),
            BulkVerb::Discard => discard_one(gait_dir, id),
        };
        match result {
            Ok(warning) => {
                acted.push(id.clone());
                if let Some(w) = warning {
                    warnings.push((id.clone(), w));
                }
            }
            Err(e) => failed.push((id.clone(), e.to_string())),
        }
    }

    Ok(BulkOutcome {
        verb: plan.verb,
        eligible: plan.eligible.clone(),
        skipped: plan.skipped.clone(),
        acted,
        failed,
        warnings,
    })
}

/// Persist one session. `Ok(Some(msg))` means the merge succeeded but the
/// checkpoint db could not be removed — a non-fatal warning for the caller to
/// render, not a print here (Wall 1).
fn persist_one(gait_dir: &Path, id: &str) -> Result<Option<String>, OpError> {
    let repos = gait_state::git::Repos::production();
    repos.traces.finish_session(gait_dir, id)?;
    if let Err(e) = repos.checkpoints.remove(gait_dir, id) {
        return Ok(Some(format!(
            "failed to remove checkpoint db for {}: {}",
            id, e
        )));
    }
    Ok(None)
}

fn discard_one(gait_dir: &Path, id: &str) -> Result<Option<String>, OpError> {
    gait_state::git::Repos::production()
        .traces
        .discard_session(gait_dir, id)?;
    Ok(None)
}

/// Walk `sessions/*`; classify each dir as eligible (idle + not running) or
/// skipped-with-reason. PURE — no writes, no exit, no I/O. Moved from
/// `cli/session.rs` as-is for parity.
type BulkTargets = (Vec<String>, Vec<(String, String)>);

fn classify_bulk_targets(gait_dir: &Path) -> Result<BulkTargets, OpError> {
    use gait_state::session::log::{session_state, SessionState};

    let sessions_dir = gait_dir.join(".active");
    let mut eligible: Vec<String> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();

    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(_) => return Ok((eligible, skipped)),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };
        let tx_path = gait_state::session::live::log_path(gait_dir, &id);
        // A session with no log.jsonl is not a started session. `read_events` treats a
        // MISSING file as empty => `session_state` returns Idle, which would wrongly
        // mark such a dir eligible — so skip it explicitly here.
        if !tx_path.exists() {
            continue;
        }
        let reason: Option<String> = match session_state(&tx_path) {
            Ok(SessionState::Idle) => {
                if gait_state::session::SessionLock::is_running(&path) {
                    Some("running".to_string())
                } else {
                    None
                }
            }
            Ok(SessionState::AwaitingPermission { .. }) => Some("awaiting-permission".to_string()),
            Ok(SessionState::AwaitingClarify { .. }) => Some("awaiting-clarify".to_string()),
            Ok(SessionState::AwaitingInform { .. }) => Some("awaiting-inform".to_string()),
            Ok(SessionState::AwaitingSandboxUnlock { .. }) => Some("awaiting-unlock".to_string()),
            Err(_) => continue,
        };
        match reason {
            None => eligible.push(id),
            Some(r) => skipped.push((id, r)),
        }
    }
    eligible.sort();
    skipped.sort();
    Ok((eligible, skipped))
}

// -----------------------------------------------------------------------------
// tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gait_state::git::Repos;
    use tempfile::TempDir;

    /// Build a gait-dir with one committed session worktree; returns the tempdir.
    /// Sets up a git repo, switches the initial branch to `main`, and creates
    /// a session worktree via `Repos::production().traces`.
    fn ws_with_session(id: &str) -> TempDir {
        let ws = TempDir::new().unwrap();
        let repo = git2::Repository::init(ws.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "t").unwrap();
        cfg.set_str("user.email", "t@t.com").unwrap();
        let sig = git2::Signature::now("t", "t@t.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        drop(tree);
        repo.find_branch("master", git2::BranchType::Local)
            .or_else(|_| repo.find_branch("main", git2::BranchType::Local))
            .and_then(|mut b| {
                b.rename("main", false)?;
                Ok(())
            })
            .ok();
        let traces = Repos::production().traces;
        traces.create_session_worktree(ws.path(), id).unwrap();
        ws
    }

    /// Build a git-initialized gait-dir with no sessions at all (just the repo
    /// plus an initial commit). `list_active_sessions` requires a real git repo
    /// to walk, so a bare (non-git) `TempDir` is not a valid "no sessions" fixture.
    fn ws_with_no_sessions() -> TempDir {
        let ws = TempDir::new().unwrap();
        let repo = git2::Repository::init(ws.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "t").unwrap();
        cfg.set_str("user.email", "t@t.com").unwrap();
        let sig = git2::Signature::now("t", "t@t.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        ws
    }

    /// Empty log => Idle, classify-eligible.
    fn write_empty_log(ws: &Path, id: &str) {
        let log_path = gait_state::session::live::log_path(ws, id);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(log_path, "").unwrap();
    }

    /// Append one event per timestamp in `timestamps` (real `UserText`
    /// bodies via `wire::append` so `content_hash`/version/scope are valid),
    /// then patch each line's `ts` field to the requested value. `read_events`
    /// only checks `version`/scope-prefix, not `content_hash`, so a patched
    /// `ts` is safe for these ordering/timestamp-only tests.
    fn write_events_with_ts(ws: &Path, id: &str, timestamps: &[u64]) {
        let log_path = gait_state::session::live::log_path(ws, id);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        for _ in timestamps {
            gait_state::proto::wire::append(
                &log_path,
                gait_state::proto::events::user_text("x".into()),
            )
            .unwrap();
        }
        let text = std::fs::read_to_string(&log_path).unwrap();
        let mut lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        for (line, ts) in lines.iter_mut().zip(timestamps) {
            line["ts"] = serde_json::json!(ts);
        }
        let new_text: String = lines
            .iter()
            .map(|v| serde_json::to_string(v).unwrap() + "\n")
            .collect();
        std::fs::write(&log_path, new_text).unwrap();
    }

    // -- list --

    #[test]
    fn list_returns_active_sessions() {
        let ws = ws_with_session("slist01");
        let out = list(ws.path(), None).unwrap();
        assert!(out.active.iter().any(|r| r.session_id == "slist01"));
    }

    #[test]
    fn list_empty_when_no_sessions() {
        let ws = ws_with_no_sessions();
        let out = list(ws.path(), None).unwrap();
        assert!(out.active.is_empty());
    }

    /// A session with no log yet reports `None` timestamps rather than
    /// failing the whole listing.
    #[test]
    fn list_reports_none_timestamps_for_session_with_no_log() {
        let ws = ws_with_session("slist02");
        let out = list(ws.path(), None).unwrap();
        let row = out
            .active
            .iter()
            .find(|r| r.session_id == "slist02")
            .expect("row present");
        assert_eq!(row.created_at, None);
        assert_eq!(row.updated_at, None);
    }

    /// Sessions with a log sort most-recently-updated first.
    #[test]
    fn list_sorts_most_recently_updated_first() {
        let ws = ws_with_session("slist03");
        Repos::production()
            .traces
            .create_session_worktree(ws.path(), "slist04")
            .unwrap();
        write_events_with_ts(ws.path(), "slist03", &[1_000, 2_000]);
        write_events_with_ts(ws.path(), "slist04", &[5_000, 9_000]);

        let out = list(ws.path(), None).unwrap();
        let ids: Vec<&str> = out.active.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, vec!["slist04", "slist03"]);
    }

    /// `last` truncates to the N most-recently-updated sessions.
    #[test]
    fn list_last_truncates_to_n_most_recent() {
        let ws = ws_with_session("slist05");
        Repos::production()
            .traces
            .create_session_worktree(ws.path(), "slist06")
            .unwrap();
        write_events_with_ts(ws.path(), "slist05", &[1_000]);
        write_events_with_ts(ws.path(), "slist06", &[9_000]);

        let out = list(ws.path(), Some(1)).unwrap();
        assert_eq!(out.active.len(), 1);
        assert_eq!(out.active[0].session_id, "slist06");
    }

    // -- ps --

    #[test]
    fn ps_classifies_idle_session() {
        let ws = ws_with_session("sps0001");
        write_empty_log(ws.path(), "sps0001");
        let out = ps(ws.path()).unwrap();
        let row = out
            .rows
            .iter()
            .find(|r| r.session_id == "sps0001")
            .expect("row present");
        assert_eq!(row.state, "idle");
    }

    /// A session whose log folds to `Idle` but whose `.active/<id>/lock` names a
    /// live PID must report `running`, not `idle` — the live turn is still
    /// holding the lock between log-visible states.
    #[test]
    fn ps_reports_running_when_lock_is_live() {
        let ws = ws_with_session("sps0002");
        write_empty_log(ws.path(), "sps0002");
        // Lock held by *this* (live) process => is_running() is true.
        let lock = gait_state::session::live::worktree_dir(ws.path(), "sps0002").join("lock");
        std::fs::write(&lock, std::process::id().to_string()).unwrap();

        let out = ps(ws.path()).unwrap();
        let row = out
            .rows
            .iter()
            .find(|r| r.session_id == "sps0002")
            .expect("row present");
        assert_eq!(row.state, "running", "idle-in-log but live lock => running");
    }

    #[test]
    fn ps_returns_empty_when_no_active_dir() {
        let ws = TempDir::new().unwrap();
        let out = ps(ws.path()).unwrap();
        assert!(out.rows.is_empty());
    }

    // -- stats --

    #[test]
    fn stats_returns_zero_for_empty_log() {
        let ws = ws_with_session("sstat01");
        write_empty_log(ws.path(), "sstat01");
        let out = stats(ws.path(), "sstat01").unwrap();
        assert_eq!(out.input, 0);
        assert_eq!(out.output, 0);
        assert_eq!(out.cache_read, 0);
        assert_eq!(out.cache_write, 0);
        assert_eq!(out.total, 0);
    }

    // -- classify_bulk / bulk_act (the three plan tests) --

    /// Test 1: pure preview — eligible/skipped classification writes nothing.
    #[test]
    fn classify_bulk_does_not_mutate() {
        let ws = ws_with_session("sbulk01");
        write_empty_log(ws.path(), "sbulk01");
        let before_log_exists = gait_state::session::live::log_path(ws.path(), "sbulk01").exists();
        let before_dir_exists =
            gait_state::session::live::worktree_dir(ws.path(), "sbulk01").exists();
        let plan = classify_bulk(ws.path(), BulkVerb::Persist).unwrap();
        assert_eq!(plan.eligible, vec!["sbulk01".to_string()]);
        assert!(plan.skipped.is_empty());
        // No side effects: log + dir untouched, eligible list matches.
        assert!(before_log_exists);
        assert!(before_dir_exists);
    }

    /// Test 2: bulk_act with confirmed=false returns OpError::ConfirmationRequired.
    #[test]
    fn bulk_act_refuses_without_confirmation() {
        let ws = ws_with_session("sbulk02");
        write_empty_log(ws.path(), "sbulk02");
        let plan = classify_bulk(ws.path(), BulkVerb::Persist).unwrap();
        let err = bulk_act(ws.path(), &plan, false, false).unwrap_err();
        assert!(matches!(err, OpError::ConfirmationRequired(_)));
        // Still present: the act loop never ran.
        assert!(gait_state::session::live::worktree_dir(ws.path(), "sbulk02").exists());
    }

    /// Test 3: bulk_act with confirmed=true performs the act.
    #[test]
    fn bulk_act_persists_eligible_set_when_confirmed() {
        let ws = ws_with_session("sbulk03");
        write_empty_log(ws.path(), "sbulk03");
        let output =
            gait_state::session::live::outputs_dir(ws.path(), "sbulk03").join("shell/full.txt");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, "runtime-only").unwrap();
        let plan = classify_bulk(ws.path(), BulkVerb::Persist).unwrap();
        let outcome = bulk_act(ws.path(), &plan, true, true).unwrap();
        assert_eq!(outcome.acted, vec!["sbulk03".to_string()]);
        assert!(outcome.failed.is_empty());
        // The session's branch is now merged into main — the worktree dir is gone.
        assert!(!gait_state::session::live::worktree_dir(ws.path(), "sbulk03").exists());
        assert!(!output.exists(), "persist must remove runtime outputs");
        let repo = git2::Repository::open(ws.path()).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        assert!(
            tree.get_path(std::path::Path::new("outputs")).is_err(),
            "persist must not merge runtime outputs into main"
        );
    }

    #[test]
    fn bulk_act_discards_eligible_set_when_confirmed() {
        let ws = ws_with_session("sbulk04");
        write_empty_log(ws.path(), "sbulk04");
        let output =
            gait_state::session::live::outputs_dir(ws.path(), "sbulk04").join("shell/full.txt");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, "runtime-only").unwrap();
        let plan = classify_bulk(ws.path(), BulkVerb::Discard).unwrap();
        let outcome = bulk_act(ws.path(), &plan, true, true).unwrap();
        assert_eq!(outcome.acted, vec!["sbulk04".to_string()]);
        assert!(outcome.failed.is_empty());
        // discard_session removes the worktree dir without merging into main.
        assert!(!gait_state::session::live::worktree_dir(ws.path(), "sbulk04").exists());
        assert!(!output.exists(), "discard must remove runtime outputs");
    }

    /// Regression test (2026-09-08): a bulk discard of more than one session
    /// must refuse without `force`, even when `confirmed` is true — a single
    /// blanket `--yes` must not be enough to wipe out many sessions at once.
    #[test]
    fn bulk_act_discard_of_multiple_sessions_requires_force() {
        let ws = ws_with_session("sforce01");
        write_empty_log(ws.path(), "sforce01");
        Repos::production()
            .traces
            .create_session_worktree(ws.path(), "sforce02")
            .unwrap();
        write_empty_log(ws.path(), "sforce02");

        let plan = classify_bulk(ws.path(), BulkVerb::Discard).unwrap();
        assert_eq!(plan.eligible.len(), 2, "fixture must have 2 eligible sessions");

        // confirmed=true, force=false: refused, nothing touched.
        let err = bulk_act(ws.path(), &plan, true, false).unwrap_err();
        assert!(matches!(err, OpError::ConfirmationRequired(_)));
        assert!(gait_state::session::live::worktree_dir(ws.path(), "sforce01").exists());
        assert!(gait_state::session::live::worktree_dir(ws.path(), "sforce02").exists());

        // confirmed=true, force=true: proceeds.
        let outcome = bulk_act(ws.path(), &plan, true, true).unwrap();
        assert_eq!(outcome.acted.len(), 2);
    }

    /// A single-session discard (the common case: `discard <id>`, or a
    /// `--all` plan that happens to match exactly one session) is NOT
    /// gated on `force` — only a genuinely bulk (>1) discard is.
    #[test]
    fn bulk_act_discard_of_a_single_session_does_not_require_force() {
        let ws = ws_with_session("sforce03");
        write_empty_log(ws.path(), "sforce03");
        let plan = classify_bulk(ws.path(), BulkVerb::Discard).unwrap();
        assert_eq!(plan.eligible.len(), 1);
        let outcome = bulk_act(ws.path(), &plan, true, false).unwrap();
        assert_eq!(outcome.acted, vec!["sforce03".to_string()]);
    }

    // -- additional classify_bulk tests (parity with cli/session.rs) --

    #[test]
    fn classify_skips_running_session() {
        let ws = ws_with_session("srun01");
        write_empty_log(ws.path(), "srun01");
        // Our own live PID in the lock => running.
        std::fs::write(
            gait_state::session::live::worktree_dir(ws.path(), "srun01").join("lock"),
            std::process::id().to_string(),
        )
        .unwrap();
        let plan = classify_bulk(ws.path(), BulkVerb::Persist).unwrap();
        assert!(plan.eligible.is_empty());
        assert_eq!(
            plan.skipped,
            vec![("srun01".to_string(), "running".to_string())]
        );
    }

    #[test]
    fn classify_omits_dir_without_log() {
        let ws = ws_with_session("snolog");
        let plan = classify_bulk(ws.path(), BulkVerb::Persist).unwrap();
        assert!(!plan.eligible.contains(&"snolog".to_string()));
        assert!(plan.skipped.iter().all(|(id, _)| id != "snolog"));
    }

    #[test]
    fn classify_missing_sessions_dir_is_empty() {
        let ws = TempDir::new().unwrap();
        let plan = classify_bulk(ws.path(), BulkVerb::Persist).unwrap();
        assert!(plan.eligible.is_empty() && plan.skipped.is_empty());
    }

    // -- undo (smoke + list mode) --

    #[test]
    fn undo_list_returns_turns_no_mutation() {
        let ws = ws_with_session("sundo01");
        write_empty_log(ws.path(), "sundo01");
        let out = undo(UndoParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sundo01".to_string(),
            turn: None,
            code: false,
            transcript: false,
            list: true,
        })
        .unwrap();
        // Empty log => no RunComplete events => empty list.
        assert!(out.turns.is_empty());
        assert!(!out.transcript_undone);
        assert!(!out.code_undone);
    }

    #[test]
    fn undo_rejects_list_and_turn() {
        let ws = ws_with_session("sundo02");
        let err = undo(UndoParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sundo02".to_string(),
            turn: Some(1),
            code: false,
            transcript: false,
            list: true,
        })
        .unwrap_err();
        assert!(matches!(err, OpError::Conflict(_)));
    }

    #[test]
    fn undo_requires_turn_or_list() {
        let ws = ws_with_session("sundo03");
        let err = undo(UndoParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sundo03".to_string(),
            turn: None,
            code: false,
            transcript: false,
            list: false,
        })
        .unwrap_err();
        assert!(matches!(err, OpError::Conflict(_)));
    }

    // -- undo (real restores) --
    //
    // Fixtures mirror what the engine writes: RunStart/RunComplete events in the
    // session log, and one `session(<id>): run <K> turn <T>` commit per turn
    // process (src/turn/spine/lifecycle.rs). A stub CheckpointRepo records which
    // shas would be restored, so the code half can be asserted without a codebase.

    use gait_proto::envelope::EventScope;
    use gait_state::git::checkpoint::{CheckpointBoundary, CheckpointRepo};
    use gait_state::proto::gait::log::v1 as pb;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubCheckpoints {
        restored: Mutex<Vec<String>>,
    }

    impl CheckpointRepo for StubCheckpoints {
        fn init(&self, _: &Path, _: &str, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn checkpoint(
            &self,
            _: &Path,
            _: &str,
            _: &Path,
            _: CheckpointBoundary,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn restore_to(&self, _: &Path, _: &str, _: &Path, sha: &str) -> anyhow::Result<()> {
            self.restored.lock().unwrap().push(sha.to_string());
            Ok(())
        }
        fn remove(&self, _: &Path, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn diff_between(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: &str,
        ) -> anyhow::Result<Option<pb::ChangeSummary>> {
            Ok(None)
        }
    }

    enum End {
        Complete,
        Cancelled,
    }

    /// Write the SessionStart event (carries `codebase_path`) and commit nothing.
    fn start_session_log(ws: &Path, id: &str) {
        use gait_state::session::log::{init_session, InitSessionOpts};
        let log_path = gait_state::session::live::log_path(ws, id);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        init_session(InitSessionOpts {
            log_path,
            id: id.to_string(),
            model: String::new(),
            codebase_hash: String::new(),
            codebase_dirty: false,
            assigned_by: None,
            codebase_path: Some("/code".to_string()),
            notify_done: false,
        })
        .unwrap();
    }

    fn log_run_start(ws: &Path, id: &str, run: u32, baseline: Option<&str>) {
        gait_proto::wire::write_to_transcript(
            &gait_state::session::live::log_path(ws, id),
            gait_state::proto::events::run_start(pb::RunStart {
                run_id: format!("r{run}"),
                index: run,
                baseline_checkpoint: baseline.map(str::to_string),
            }),
            EventScope::Run { run },
        )
        .unwrap();
    }

    fn log_run_end(ws: &Path, id: &str, run: u32, end: End, final_sha: Option<&str>) {
        let final_state = Some(pb::RunEndState {
            codebase: None,
            checkpoint: final_sha.map(str::to_string),
        });
        let body = match end {
            End::Complete => gait_state::proto::events::run_complete(pb::RunComplete {
                final_state,
                summary: None,
            }),
            End::Cancelled => {
                gait_state::proto::events::run_cancelled(pb::RunCancelled { final_state })
            }
        };
        gait_proto::wire::write_to_transcript(
            &gait_state::session::live::log_path(ws, id),
            body,
            EventScope::Run { run },
        )
        .unwrap();
    }

    fn commit_turn(ws: &Path, id: &str, run: u32, turn: u32) {
        Repos::production()
            .traces
            .commit_session(ws, id, &format!("session({id}): run {run} turn {turn}"))
            .unwrap();
    }

    fn head_summary(ws: &Path, id: &str) -> String {
        let repo =
            git2::Repository::open(gait_state::session::live::worktree_dir(ws, id)).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        head.summary().unwrap().to_string()
    }

    fn undo_params(ws: &Path, id: &str, turn: u32, code: bool, transcript: bool) -> UndoParams {
        UndoParams {
            gait_dir: ws.to_path_buf(),
            session_id: id.to_string(),
            turn: Some(turn),
            code,
            transcript,
            list: false,
        }
    }

    #[test]
    fn undo_transcript_resets_to_the_newest_commit_of_the_run() {
        let ws = ws_with_session("sundo10");
        let id = "sundo10";
        start_session_log(ws.path(), id);
        // Run 0 spans two turn commits (a pause then a resume); run 1 follows.
        log_run_start(ws.path(), id, 0, Some("base0"));
        commit_turn(ws.path(), id, 0, 0);
        log_run_end(ws.path(), id, 0, End::Complete, Some("end0"));
        commit_turn(ws.path(), id, 0, 1);
        log_run_start(ws.path(), id, 1, Some("end0"));
        log_run_end(ws.path(), id, 1, End::Complete, Some("end1"));
        commit_turn(ws.path(), id, 1, 0);

        let cp = StubCheckpoints::default();
        let out = undo_with(undo_params(ws.path(), id, 1, false, true), &cp).unwrap();

        assert!(out.transcript_undone && !out.code_undone);
        assert_eq!(head_summary(ws.path(), id), format!("session({id}): run 0 turn 1"));
        // The log really went back: run 1's events are gone.
        let events =
            gait_state::session::log::read_events(&gait_state::session::live::log_path(ws.path(), id))
                .unwrap();
        let starts = events
            .iter()
            .filter(|e| {
                matches!(
                    &e.body,
                    Some(gait_state::proto::gait::log::v1::event::Body::Payload(p))
                        if matches!(p.kind, Some(gait_state::proto::gait::log::v1::payload::Kind::RunStart(_)))
                )
            })
            .count();
        assert_eq!(starts, 1);
        assert!(cp.restored.lock().unwrap().is_empty());
    }

    #[test]
    fn undo_run_prefix_does_not_match_a_longer_run_number() {
        let ws = ws_with_session("sundo11");
        let id = "sundo11";
        start_session_log(ws.path(), id);
        log_run_start(ws.path(), id, 1, None);
        log_run_end(ws.path(), id, 1, End::Complete, None);
        commit_turn(ws.path(), id, 1, 0);
        log_run_start(ws.path(), id, 10, None);
        log_run_end(ws.path(), id, 10, End::Complete, None);
        commit_turn(ws.path(), id, 10, 0);

        // Turn 1 is run 1; the newer "run 10 turn 0" commit must not satisfy it.
        undo_with(undo_params(ws.path(), id, 1, false, true), &StubCheckpoints::default()).unwrap();
        assert_eq!(head_summary(ws.path(), id), format!("session({id}): run 1 turn 0"));
    }

    #[test]
    fn undo_turn_number_counts_completed_runs_not_run_ordinals() {
        let ws = ws_with_session("sundo12");
        let id = "sundo12";
        start_session_log(ws.path(), id);
        log_run_start(ws.path(), id, 0, None);
        log_run_end(ws.path(), id, 0, End::Complete, None);
        commit_turn(ws.path(), id, 0, 0);
        log_run_start(ws.path(), id, 1, None);
        log_run_end(ws.path(), id, 1, End::Cancelled, None); // no RunComplete
        commit_turn(ws.path(), id, 1, 0);
        log_run_start(ws.path(), id, 2, None);
        log_run_end(ws.path(), id, 2, End::Complete, None);
        commit_turn(ws.path(), id, 2, 0);
        // Move HEAD off run 2 so the reset is observable.
        log_run_start(ws.path(), id, 3, None);
        commit_turn(ws.path(), id, 3, 0);

        // The second completed run is run 2, not run 1.
        undo_with(undo_params(ws.path(), id, 2, false, true), &StubCheckpoints::default()).unwrap();
        assert_eq!(head_summary(ws.path(), id), format!("session({id}): run 2 turn 0"));
    }

    #[test]
    fn undo_missing_transcript_commit_fails_before_restoring_code() {
        let ws = ws_with_session("sundo13");
        let id = "sundo13";
        start_session_log(ws.path(), id);
        log_run_start(ws.path(), id, 0, Some("base0"));
        log_run_end(ws.path(), id, 0, End::Complete, Some("end0"));
        // No `run 0 turn N` commit exists (the engine ignores a failed commit).

        let cp = StubCheckpoints::default();
        let err = undo_with(undo_params(ws.path(), id, 1, false, false), &cp).unwrap_err();

        assert!(matches!(err, OpError::NotFound(_)), "{err:?}");
        assert!(
            cp.restored.lock().unwrap().is_empty(),
            "code must not be restored when the transcript target is missing"
        );
    }

    #[test]
    fn undo_code_only_restores_the_runs_final_checkpoint() {
        let ws = ws_with_session("sundo14");
        let id = "sundo14";
        start_session_log(ws.path(), id);
        log_run_start(ws.path(), id, 0, Some("base0"));
        log_run_end(ws.path(), id, 0, End::Complete, Some("end0"));

        let cp = StubCheckpoints::default();
        let out = undo_with(undo_params(ws.path(), id, 1, true, false), &cp).unwrap();

        assert!(out.code_undone && !out.transcript_undone);
        assert_eq!(*cp.restored.lock().unwrap(), vec!["end0".to_string()]);
    }

    #[test]
    fn undo_turn_zero_restores_the_first_baseline_code_only() {
        let ws = ws_with_session("sundo15");
        let id = "sundo15";
        start_session_log(ws.path(), id);
        log_run_start(ws.path(), id, 0, Some("base0"));
        log_run_end(ws.path(), id, 0, End::Complete, Some("end0"));
        log_run_start(ws.path(), id, 1, Some("end0"));
        log_run_end(ws.path(), id, 1, End::Complete, Some("end1"));

        let cp = StubCheckpoints::default();
        let out = undo_with(undo_params(ws.path(), id, 0, true, false), &cp).unwrap();
        assert!(out.code_undone && !out.transcript_undone);
        assert_eq!(*cp.restored.lock().unwrap(), vec!["base0".to_string()]);

        // Default scope and --transcript would reset the transcript past its
        // first commit, which deletes log.jsonl: refused, nothing restored.
        let cp = StubCheckpoints::default();
        for (code, transcript) in [(false, false), (false, true)] {
            let err = undo_with(undo_params(ws.path(), id, 0, code, transcript), &cp).unwrap_err();
            assert!(matches!(err, OpError::Conflict(_)), "{err:?}");
        }
        assert!(cp.restored.lock().unwrap().is_empty());
    }

    #[test]
    fn undo_turn_zero_without_a_recorded_baseline_is_a_conflict() {
        let ws = ws_with_session("sundo16");
        let id = "sundo16";
        start_session_log(ws.path(), id);
        log_run_start(ws.path(), id, 0, None);
        log_run_end(ws.path(), id, 0, End::Complete, Some("end0"));

        let err = undo_with(
            undo_params(ws.path(), id, 0, true, false),
            &StubCheckpoints::default(),
        )
        .unwrap_err();
        assert!(matches!(err, OpError::Conflict(_)), "{err:?}");
    }

    #[test]
    fn undo_turn_beyond_the_last_is_not_found() {
        let ws = ws_with_session("sundo17");
        let id = "sundo17";
        start_session_log(ws.path(), id);
        log_run_start(ws.path(), id, 0, None);
        log_run_end(ws.path(), id, 0, End::Complete, None);

        let err = undo_with(
            undo_params(ws.path(), id, 2, true, true),
            &StubCheckpoints::default(),
        )
        .unwrap_err();
        assert!(matches!(err, OpError::NotFound(_)), "{err:?}");
    }

    // -- shared renderer (golden tests) --

    #[test]
    fn ps_outcome_to_rows_and_json_golden() {
        let out = PsOutcome {
            rows: vec![
                PsRow {
                    session_id: "s1".to_string(),
                    state: "idle".to_string(),
                },
                PsRow {
                    session_id: "s2".to_string(),
                    state: "awaiting-permission".to_string(),
                },
            ],
        };
        assert_eq!(
            out.to_rows(),
            vec![
                ("s1".to_string(), "idle".to_string()),
                ("s2".to_string(), "awaiting-permission".to_string()),
            ]
        );
        assert_eq!(
            out.to_json(false),
            "[{\"session_id\":\"s1\",\"state\":\"idle\"},{\"session_id\":\"s2\",\"state\":\"awaiting-permission\"}]"
        );
        assert_eq!(
            out.to_json(true),
            "[\n  {\n    \"session_id\": \"s1\",\n    \"state\": \"idle\"\n  },\n  {\n    \"session_id\": \"s2\",\n    \"state\": \"awaiting-permission\"\n  }\n]"
        );
    }

    #[test]
    fn stats_outcome_to_json_golden() {
        let out = StatsOutcome {
            input: 10,
            output: 20,
            cache_read: 3,
            cache_write: 4,
            compaction_input: 0,
            compaction_output: 0,
            compaction_cache_read: 0,
            compaction_cache_write: 0,
            total: 37,
        };
        assert_eq!(
            out.to_json(false),
            "{\"input\":10,\"output\":20,\"cache_read\":3,\"cache_write\":4,\"compaction_input\":0,\"compaction_output\":0,\"compaction_cache_read\":0,\"compaction_cache_write\":0,\"total\":37}"
        );
        assert_eq!(
            out.to_json(true),
            "{\n  \"input\": 10,\n  \"output\": 20,\n  \"cache_read\": 3,\n  \"cache_write\": 4,\n  \"compaction_input\": 0,\n  \"compaction_output\": 0,\n  \"compaction_cache_read\": 0,\n  \"compaction_cache_write\": 0,\n  \"total\": 37\n}"
        );
    }

    #[test]
    fn bulk_plan_to_json_golden() {
        let plan = BulkPlan {
            verb: BulkVerb::Persist,
            eligible: vec!["s1".to_string()],
            skipped: vec![("s2".to_string(), "running".to_string())],
        };
        assert_eq!(
            plan.to_json(false),
            "{\"eligible\":[\"s1\"],\"skipped\":[{\"session_id\":\"s2\",\"state\":\"running\"}],\"acted\":[],\"failed\":[]}"
        );
        assert_eq!(
            plan.to_json(true),
            "{\n  \"eligible\": [\n    \"s1\"\n  ],\n  \"skipped\": [\n    {\n      \"session_id\": \"s2\",\n      \"state\": \"running\"\n    }\n  ],\n  \"acted\": [],\n  \"failed\": []\n}"
        );
    }

    #[test]
    fn bulk_outcome_to_json_golden() {
        let outcome = BulkOutcome {
            verb: BulkVerb::Discard,
            eligible: vec!["s1".to_string()],
            skipped: Vec::new(),
            acted: vec!["s1".to_string()],
            failed: Vec::new(),
            warnings: Vec::new(),
        };
        assert_eq!(
            outcome.to_json(false),
            "{\"eligible\":[\"s1\"],\"skipped\":[],\"acted\":[\"s1\"],\"failed\":[]}"
        );
        assert_eq!(
            outcome.to_json(true),
            "{\n  \"eligible\": [\n    \"s1\"\n  ],\n  \"skipped\": [],\n  \"acted\": [\n    \"s1\"\n  ],\n  \"failed\": []\n}"
        );
    }

    // -- new JSON shapes: list (sessions) and undo --

    #[test]
    fn list_outcome_to_json_golden() {
        let out = ListOutcome {
            active: vec![SessionRow {
                session_id: "s1".to_string(),
                created_at: Some(1_000),
                updated_at: Some(2_000),
            }],
        };
        assert_eq!(
            out.to_json(false),
            "{\"active\":[{\"session_id\":\"s1\",\"created_at\":1000,\"updated_at\":2000}]}"
        );
        assert_eq!(
            out.to_json(true),
            "{\n  \"active\": [\n    {\n      \"session_id\": \"s1\",\n      \"created_at\": 1000,\n      \"updated_at\": 2000\n    }\n  ]\n}"
        );
    }

    #[test]
    fn list_outcome_to_json_empty() {
        let out = ListOutcome { active: vec![] };
        assert_eq!(out.to_json(false), "{\"active\":[]}");
    }

    #[test]
    fn undo_outcome_to_json_list_mode_golden() {
        let out = UndoOutcome {
            turns: vec![
                UndoTurn {
                    turn: 1,
                    summary: "did a thing".to_string(),
                    checkpoint_short: Some("abcd1234".to_string()),
                },
                UndoTurn {
                    turn: 2,
                    summary: "did another".to_string(),
                    checkpoint_short: None,
                },
            ],
            transcript_undone: false,
            code_undone: false,
            checkpoint_short: None,
        };
        assert_eq!(
            out.to_json(false),
            "{\"turns\":[{\"turn\":1,\"summary\":\"did a thing\",\"checkpoint_short\":\"abcd1234\"},{\"turn\":2,\"summary\":\"did another\",\"checkpoint_short\":null}],\"transcript_undone\":false,\"code_undone\":false,\"checkpoint_short\":null}"
        );
    }

    #[test]
    fn undo_outcome_to_json_act_mode_golden() {
        let out = UndoOutcome {
            turns: vec![],
            transcript_undone: true,
            code_undone: true,
            checkpoint_short: Some("abcd1234".to_string()),
        };
        assert_eq!(
            out.to_json(false),
            "{\"turns\":[],\"transcript_undone\":true,\"code_undone\":true,\"checkpoint_short\":\"abcd1234\"}"
        );
    }

    #[test]
    fn undo_unknown_turn_is_not_found() {
        let ws = ws_with_session("sundo04");
        write_empty_log(ws.path(), "sundo04");
        let err = undo(UndoParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sundo04".to_string(),
            turn: Some(7),
            code: false,
            transcript: false,
            list: false,
        })
        .unwrap_err();
        assert!(matches!(err, OpError::NotFound(_)));
    }

    // -- summary (cmd_log) --

    /// Append a `TurnResult` (TextDone) then a `RunComplete` (with a
    /// `RunSummary`) to `id`'s log — the minimal two-event fixture `summary`
    /// reads.
    fn write_run_complete_fixture(ws: &Path, id: &str, message: &str, checkpoint: &str) {
        use gait_state::proto::events::{run_complete, turn_result};
        use gait_state::proto::gait::log::v1::{
            turn_done, turn_result::Outcome, ChangeSummary, RunComplete, RunEndState, RunSummary,
            TextDone, ToolCount, TurnDone, TurnResult,
        };
        let log_path = gait_state::session::live::log_path(ws, id);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        gait_state::proto::wire::append(
            &log_path,
            turn_result(TurnResult {
                outcome: Some(Outcome::Done(TurnDone {
                    output: Some(turn_done::Output::Text(TextDone {
                        message: message.to_string(),
                    })),
                })),
                ..Default::default()
            }),
        )
        .unwrap();
        gait_state::proto::wire::append(
            &log_path,
            run_complete(RunComplete {
                final_state: Some(RunEndState {
                    checkpoint: Some(checkpoint.to_string()),
                    ..Default::default()
                }),
                summary: Some(RunSummary {
                    changes: Some(ChangeSummary {
                        file_count: 2,
                        added_lines: Some(10),
                        removed_lines: Some(3),
                        ..Default::default()
                    }),
                    tools: vec![ToolCount {
                        name: "shell".to_string(),
                        count: 3,
                    }],
                    ..Default::default()
                }),
            }),
        )
        .unwrap();
    }

    #[test]
    fn summary_empty_when_no_completed_run() {
        let ws = ws_with_session("ssum01");
        write_empty_log(ws.path(), "ssum01");
        let out = summary(ws.path(), "ssum01").unwrap();
        assert_eq!(out.turn, None);
        assert_eq!(out.message, "");
        assert_eq!(out.checkpoint_short, None);
        assert_eq!(out.file_count, 0);
        assert!(out.tools.is_empty());
    }

    #[test]
    fn summary_returns_last_run_completes_details() {
        let ws = ws_with_session("ssum02");
        write_run_complete_fixture(ws.path(), "ssum02", "did the thing", "abcdef123456");
        let out = summary(ws.path(), "ssum02").unwrap();
        assert_eq!(out.turn, Some(1));
        assert_eq!(out.message, "did the thing");
        assert_eq!(out.checkpoint_short, Some("abcdef12".to_string()));
        assert_eq!(out.file_count, 2);
        assert_eq!(out.added_lines, Some(10));
        assert_eq!(out.removed_lines, Some(3));
        assert_eq!(
            out.tools,
            vec![SummaryToolCount {
                name: "shell".to_string(),
                count: 3
            }]
        );
    }

    #[test]
    fn summary_outcome_to_json_golden() {
        let out = SessionSummaryOutcome {
            turn: Some(2),
            message: "did stuff".to_string(),
            checkpoint_short: Some("abcd1234".to_string()),
            file_count: 1,
            added_lines: Some(5),
            removed_lines: None,
            tools: vec![SummaryToolCount {
                name: "shell".to_string(),
                count: 1,
            }],
        };
        assert_eq!(
            out.to_json(false),
            "{\"turn\":2,\"message\":\"did stuff\",\"checkpoint_short\":\"abcd1234\",\"file_count\":1,\"added_lines\":5,\"removed_lines\":null,\"tools\":[{\"name\":\"shell\",\"count\":1}]}"
        );
    }
}
