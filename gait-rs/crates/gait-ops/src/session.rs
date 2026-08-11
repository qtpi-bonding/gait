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

#[derive(Debug)]
pub struct ListOutcome {
    /// Active (unmerged) session ids.
    pub active: Vec<String>,
}

/// Walk `gait_dir`'s `session/*` branches and return the ones main is not a
/// descendant of. Read-only; no log/branch writes.
pub fn list(gait_dir: &Path) -> Result<ListOutcome, OpError> {
    let active = gait_state::git::Repos::production()
        .traces
        .list_active_sessions(gait_dir)?;
    Ok(ListOutcome { active })
}

// -----------------------------------------------------------------------------
// ps  (cmd_ps)
// -----------------------------------------------------------------------------

#[derive(Debug)]
pub struct PsRow {
    pub session_id: String,
    pub state: String,
}

#[derive(Debug)]
pub struct PsOutcome {
    pub rows: Vec<PsRow>,
}

impl PsOutcome {
    /// `(session_id, state)` rows, in the order returned by `ps` — the single
    /// source of truth for the tab-separated table both the CLI and (future)
    /// REPL print. No I/O; returns data only (Wall 1).
    pub fn to_rows(&self) -> Vec<(String, String)> {
        self.rows
            .iter()
            .map(|r| (r.session_id.clone(), r.state.clone()))
            .collect()
    }

    /// The `--json` shape: `[{"session_id": .., "state": ..}, ...]`, pretty
    /// printed. This is the API contract — CLI and any future frontend render
    /// this exact string rather than re-deriving the shape.
    pub fn to_json(&self) -> String {
        let arr: Vec<serde_json::Value> = self
            .rows
            .iter()
            .map(|r| serde_json::json!({"session_id": r.session_id, "state": r.state}))
            .collect();
        serde_json::to_string_pretty(&arr).unwrap_or_default()
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

#[derive(Debug)]
pub struct StatsOutcome {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

impl StatsOutcome {
    /// The `--json` shape: a single compact object (no `total` field, matching
    /// the pre-existing CLI output byte-for-byte). Single source of truth for
    /// the `stats --json` contract.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"input\":{},\"output\":{},\"cache_read\":{},\"cache_write\":{}}}",
            self.input, self.output, self.cache_read, self.cache_write
        )
    }
}

/// Cumulative token usage for a session, folded from the log.
pub fn stats(gait_dir: &Path, session_id: &str) -> Result<StatsOutcome, OpError> {
    use gait_state::session::log::read_events;
    use gait_state::session::retrace::total_usage;
    let tx_path = gait_state::session::live::log_path(gait_dir, session_id);
    let events = read_events(&tx_path).map_err(anyhow::Error::from)?;
    let u = total_usage(&events);
    let total = u.input + u.output + u.cache_read + u.cache_write;
    Ok(StatsOutcome {
        input: u.input,
        output: u.output,
        cache_read: u.cache_read,
        cache_write: u.cache_write,
        total,
    })
}

// -----------------------------------------------------------------------------
// undo  (cmd_undo)
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
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

#[derive(Debug)]
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

/// Port of `cmd_undo`'s two modes. `list: true` returns the turn list (no
/// mutation). Otherwise `turn` is required; default scope (no `code`/`transcript`
/// flag) acts on both axes. The op takes the `SessionLock` itself.
pub fn undo(params: UndoParams) -> Result<UndoOutcome, OpError> {
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
    let mut turns: Vec<UndoTurn> = Vec::new();
    let mut full_shas: Vec<Option<String>> = Vec::new();
    for e in &events {
        if let Some(event::Body::Payload(p)) = &e.body {
            if let Some(payload::Kind::RunComplete(complete)) = &p.kind {
                let idx = turns.len() as u32 + 1;
                let message = events
                    .iter()
                    .rev()
                    .filter(|candidate| candidate.seq < e.seq && candidate.run == e.run)
                    .find_map(|candidate| match &candidate.body {
                        Some(event::Body::Payload(payload)) => match payload.kind.as_ref() {
                            Some(payload::Kind::TurnResult(turn)) => match turn.outcome.as_ref() {
                                Some(gait_state::proto::gait::log::v1::turn_result::Outcome::Done(done)) => match done.output.as_ref() {
                                    Some(gait_state::proto::gait::log::v1::turn_done::Output::Text(text)) => Some(text.message.clone()),
                                    Some(gait_state::proto::gait::log::v1::turn_done::Output::Structured(output)) => Some(output.message.clone()),
                                    None => Some(String::new()),
                                },
                                _ => None,
                            },
                            _ => None,
                        },
                        _ => None,
                    })
                    .unwrap_or_default();
                let full = complete
                    .final_state
                    .as_ref()
                    .and_then(|state| state.checkpoint.clone());
                let checkpoint_short = full.as_deref().map(|s| s[..s.len().min(8)].to_string());
                turns.push(UndoTurn {
                    turn: idx,
                    summary: message,
                    checkpoint_short,
                });
                full_shas.push(full);
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
    let turn_idx = (turn as usize)
        .checked_sub(1)
        .ok_or_else(|| OpError::Conflict(format!("turn index must be >= 1, got {}", turn)))?;
    if turn_idx >= turns.len() {
        return Err(OpError::NotFound(format!(
            "no turn {} in session {}",
            turn, params.session_id
        )));
    }
    let turn_row = turns[turn_idx].clone();
    let full_sha = full_shas[turn_idx].clone();

    let do_code = params.code || !params.transcript;
    let do_transcript = params.transcript || !params.code;

    // Acquire the lock; refuse if a turn is in progress.
    let session_dir = gait_state::session::live::worktree_dir(&params.gait_dir, &params.session_id);
    let _lock = gait_state::session::SessionLock::acquire(&session_dir).map_err(|e| {
        OpError::SessionBusy(format!("cannot undo: {} (is a turn in progress?)", e))
    })?;

    let mut code_undone = false;
    let mut transcript_undone = false;

    if do_code {
        if let Some(sha) = full_sha.as_deref() {
            let codebase =
                gait_state::session::read_session_meta(&params.gait_dir, &params.session_id)?
                    .codebase_path
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| {
                        OpError::Conflict(
                            "session has no codebase_path; cannot undo code".to_string(),
                        )
                    })?;
            let repos = gait_state::git::Repos::production();
            repos
                .checkpoints
                .restore_to(&params.gait_dir, &params.session_id, &codebase, sha)?;
            code_undone = true;
        }
    }

    if do_transcript {
        undo_transcript_to_turn(&params.gait_dir, &params.session_id, turn)?;
        transcript_undone = true;
    }

    Ok(UndoOutcome {
        turns: Vec::new(),
        transcript_undone,
        code_undone,
        checkpoint_short: turn_row.checkpoint_short,
    })
}

fn undo_transcript_to_turn(gait_dir: &Path, session_id: &str, turn: u32) -> Result<(), OpError> {
    let session_dir = gait_state::session::live::worktree_dir(gait_dir, session_id);
    let repo = git2::Repository::open(&session_dir).map_err(anyhow::Error::from)?;
    let needle = format!("session({}): turn {}", session_id, turn);
    let mut walk = repo.revwalk().map_err(anyhow::Error::from)?;
    walk.push_head().map_err(anyhow::Error::from)?;
    let mut target: Option<git2::Oid> = None;
    for oid in walk {
        let oid = oid.map_err(anyhow::Error::from)?;
        let commit = repo.find_commit(oid).map_err(anyhow::Error::from)?;
        if commit.summary() == Some(needle.as_str()) {
            target = Some(oid);
            break;
        }
    }
    let oid = target.ok_or_else(|| {
        OpError::NotFound(format!(
            "no commit for turn {} in session {} (transcript undo applies to interactive turn sessions, not workflows)",
            turn, session_id
        ))
    })?;
    let obj = repo.find_object(oid, None).map_err(anyhow::Error::from)?;
    repo.reset(&obj, git2::ResetType::Hard, None)
        .map_err(anyhow::Error::from)?;
    Ok(())
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
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

impl BulkPlan {
    /// `--json` shape for the preview (before `bulk_act` runs): `acted`/`failed`
    /// are empty arrays.
    pub fn to_json(&self) -> String {
        render_bulk_json(&self.eligible, &self.skipped, &[], &[])
    }
}

impl BulkOutcome {
    /// `--json` shape for the completed action.
    pub fn to_json(&self) -> String {
        render_bulk_json(&self.eligible, &self.skipped, &self.acted, &self.failed)
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
pub fn bulk_act(gait_dir: &Path, plan: &BulkPlan, confirmed: bool) -> Result<BulkOutcome, OpError> {
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

    // -- list --

    #[test]
    fn list_returns_active_sessions() {
        let ws = ws_with_session("slist01");
        let out = list(ws.path()).unwrap();
        assert!(out.active.contains(&"slist01".to_string()));
    }

    #[test]
    fn list_empty_when_no_sessions() {
        let ws = ws_with_no_sessions();
        let out = list(ws.path()).unwrap();
        assert!(out.active.is_empty());
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
        let err = bulk_act(ws.path(), &plan, false).unwrap_err();
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
        let outcome = bulk_act(ws.path(), &plan, true).unwrap();
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
        let outcome = bulk_act(ws.path(), &plan, true).unwrap();
        assert_eq!(outcome.acted, vec!["sbulk04".to_string()]);
        assert!(outcome.failed.is_empty());
        // discard_session removes the worktree dir without merging into main.
        assert!(!gait_state::session::live::worktree_dir(ws.path(), "sbulk04").exists());
        assert!(!output.exists(), "discard must remove runtime outputs");
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
            out.to_json(),
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
            total: 37,
        };
        assert_eq!(
            out.to_json(),
            "{\"input\":10,\"output\":20,\"cache_read\":3,\"cache_write\":4}"
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
            plan.to_json(),
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
            outcome.to_json(),
            "{\n  \"eligible\": [\n    \"s1\"\n  ],\n  \"skipped\": [],\n  \"acted\": [\n    \"s1\"\n  ],\n  \"failed\": []\n}"
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
}
