//! ACP Adapter Phase 7 — session lifecycle: `session/list`, `session/close`,
//! `session/delete`. See docs/plans/2026-07-12-acp-adapter-phase7-design.md.
//!
//! Every handler here is a synchronous request-thread operation (daemonless
//! invariant): `close`/`delete` may cancel and *join* an in-flight turn thread
//! (via `SessionMap::remove`) but start no new long-lived process.
//!
//! Session content — which sessions exist, their cwd, their state — comes
//! from gait's persisted layer (`gait_ops::session::ps` +
//! `gait_state::session::read_session_meta`), never from the in-memory
//! `SessionMap`, which is live per-turn coordination only.
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    CloseSessionRequest, CloseSessionResponse, DeleteSessionRequest, DeleteSessionResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionInfo, SessionModeId, SessionNotification,
};
use agent_client_protocol::{Client, ConnectionTo, Responder};
use gait_ops::session::BulkVerb;

use crate::session_map::{SessionMap, discover_gait_dir_for_cwd};
use crate::turn_io::replay_updates;

/// Resolve a persisted session against the request's claimed workspace.
///
/// Unlike lifecycle operations carrying only an id, load/resume must prove the
/// session belongs to the supplied cwd before it is attached to this
/// connection's live coordination map.
fn persisted_session_for_cwd(id: &str, cwd: &Path) -> Result<PathBuf, String> {
    if !cwd.is_absolute() {
        return Err("session cwd must be absolute".to_string());
    }
    let gait_dir = discover_gait_dir_for_cwd(cwd).map_err(|e| e.to_string())?;
    let ps = gait_ops::session::ps(&gait_dir).map_err(|e| e.to_string())?;
    if !ps.rows.iter().any(|row| row.session_id == id) {
        return Err(format!("unknown active session: {id}"));
    }
    let meta = gait_state::session::read_session_meta(&gait_dir, id).map_err(|e| e.to_string())?;
    let Some(recorded_cwd) = meta.codebase_path else {
        return Err(format!("session has no recorded cwd: {id}"));
    };
    if Path::new(&recorded_cwd) != cwd {
        return Err(format!("session cwd does not match request: {id}"));
    }
    Ok(gait_dir)
}

fn validate_load_options(
    mcp_servers_empty: bool,
    additional_directories_empty: bool,
) -> Result<(), String> {
    if !mcp_servers_empty {
        return Err("ACP MCP servers are not supported for session load/resume".to_string());
    }
    if !additional_directories_empty {
        return Err(
            "ACP additional directories are not supported for session load/resume".to_string(),
        );
    }
    Ok(())
}

fn restored_modes(
    gait_dir: &Path,
    events: &[gait_core::proto::gait::log::v1::Event],
) -> agent_client_protocol::schema::v1::SessionModeState {
    let mut modes = crate::handlers::advertise_modes(gait_dir);
    let mut mutations = gait_state::session::retrace::active_mutations(events);
    gait_state::session::retrace::apply_cascade(&mut mutations, events);
    if let Some(mode) = gait_state::session::retrace::active_mode(events, &mutations) {
        modes.current_mode_id = SessionModeId::new(mode.name);
    }
    modes
}

/// `session/load`: attach a persisted session and replay its renderable
/// mutation-applied history before responding.
pub fn load_session(
    map: &SessionMap,
    req: LoadSessionRequest,
    responder: Responder<LoadSessionResponse>,
    cx: ConnectionTo<Client>,
) -> Result<(), agent_client_protocol::Error> {
    let sid = req.session_id.0.to_string();
    if let Err(e) = validate_load_options(
        req.mcp_servers.is_empty(),
        req.additional_directories.is_empty(),
    ) {
        return responder.respond_with_internal_error(e);
    }
    let gait_dir = match persisted_session_for_cwd(&sid, &req.cwd) {
        Ok(dir) => dir,
        Err(e) => return responder.respond_with_internal_error(e),
    };
    let events = match gait_state::session::log::read_events(&gait_state::session::live::log_path(
        &gait_dir, &sid,
    )) {
        Ok(events) => events,
        Err(e) => return responder.respond_with_internal_error(e.to_string()),
    };
    let updates = replay_updates(&events);
    let inserted = match map.attach_existing(gait_dir.clone(), sid.clone(), req.cwd.clone()) {
        Ok(inserted) => inserted,
        Err(e) => return responder.respond_with_internal_error(e),
    };
    for update in updates {
        if let Err(e) =
            cx.send_notification(SessionNotification::new(req.session_id.clone(), update))
        {
            if inserted {
                map.remove(&sid);
            }
            return responder.respond_with_internal_error(format!("failed to replay session: {e}"));
        }
    }
    responder.respond(LoadSessionResponse::new().modes(Some(restored_modes(&gait_dir, &events))))
}

/// `session/resume`: attach a persisted session without replaying history.
pub fn resume_session(
    map: &SessionMap,
    req: ResumeSessionRequest,
    responder: Responder<ResumeSessionResponse>,
) -> Result<(), agent_client_protocol::Error> {
    let sid = req.session_id.0.to_string();
    if let Err(e) = validate_load_options(
        req.mcp_servers.is_empty(),
        req.additional_directories.is_empty(),
    ) {
        return responder.respond_with_internal_error(e);
    }
    let gait_dir = match persisted_session_for_cwd(&sid, &req.cwd) {
        Ok(dir) => dir,
        Err(e) => return responder.respond_with_internal_error(e),
    };
    let events = match gait_state::session::log::read_events(&gait_state::session::live::log_path(
        &gait_dir, &sid,
    )) {
        Ok(events) => events,
        Err(e) => return responder.respond_with_internal_error(e.to_string()),
    };
    if let Err(e) = map.attach_existing(gait_dir.clone(), sid, req.cwd) {
        return responder.respond_with_internal_error(e);
    }
    responder.respond(ResumeSessionResponse::new().modes(Some(restored_modes(&gait_dir, &events))))
}

/// Locate the gait-dir a bare session id belongs to (design §4):
/// 1. `map.resolve(id)` — minted this connection.
/// 2. Otherwise scan `map.known_gait_dirs()` for the first dir whose `ps`
///    lists the id (a persisted session this connection didn't mint).
/// 3. `None` — unknown session.
pub fn gait_dir_for_session(map: &SessionMap, id: &str) -> Option<PathBuf> {
    if let Some((gait_dir, _codebase)) = map.resolve(id) {
        return Some(gait_dir);
    }
    for gd in map.known_gait_dirs() {
        if let Ok(ps) = gait_ops::session::ps(&gd)
            && ps.rows.iter().any(|r| r.session_id == id)
        {
            return Some(gd);
        }
    }
    None
}

/// `session/list` — enumerate persisted `.active/<id>` sessions across the
/// gait-dirs implied by the request (design §5).
pub fn list_sessions(
    map: &SessionMap,
    req: ListSessionsRequest,
    responder: Responder<ListSessionsResponse>,
) -> Result<(), agent_client_protocol::Error> {
    let gait_dirs: Vec<PathBuf> = match &req.cwd {
        Some(cwd) => match discover_gait_dir_for_cwd(cwd) {
            Ok(dir) => vec![dir],
            // A cwd with no linked gait-dir is not an error: it has no sessions.
            Err(_) => return responder.respond(ListSessionsResponse::new(vec![])),
        },
        None => map.known_gait_dirs(),
    };

    let mut infos = Vec::new();
    for gd in gait_dirs {
        let Ok(ps) = gait_ops::session::ps(&gd) else {
            continue;
        };
        for row in ps.rows {
            let meta = gait_state::session::read_session_meta(&gd, &row.session_id);
            let Some(cwd) = meta.ok().and_then(|m| m.codebase_path) else {
                continue;
            };
            if let Some(want) = &req.cwd
                && Path::new(&cwd) != want.as_path()
            {
                continue;
            }
            infos.push(SessionInfo::new(row.session_id, PathBuf::from(cwd)));
        }
    }
    responder.respond(ListSessionsResponse::new(infos))
}

/// `session/close` — persist (merge `session/<id>` into main), no interactive
/// consent: the editor's close request *is* the consent (design §6).
pub fn close_session(
    map: &SessionMap,
    req: CloseSessionRequest,
    responder: Responder<CloseSessionResponse>,
) -> Result<(), agent_client_protocol::Error> {
    let sid = req.session_id.0.to_string();
    let Some(gait_dir) = gait_dir_for_session(map, &sid) else {
        return responder.respond_with_internal_error(format!("unknown session: {sid}"));
    };

    // Cancel + join any live turn on this connection FIRST — bulk_act takes
    // the SessionLock and classify_bulk marks a running session ineligible,
    // so the turn must be settled before we classify/act (design §6 ordering).
    map.remove(&sid);

    let plan = match gait_ops::session::classify_bulk(&gait_dir, BulkVerb::Persist) {
        Ok(p) => p,
        Err(e) => return responder.respond_with_internal_error(e.to_string()),
    };
    if !plan.eligible.iter().any(|s| s == &sid) {
        let why = plan
            .skipped
            .iter()
            .find(|(id, _)| id == &sid)
            .map(|(_, r)| r.clone())
            .unwrap_or_else(|| "session is not eligible to persist".to_string());
        return responder.respond_with_internal_error(format!("cannot close {sid}: {why}"));
    }
    let one = gait_ops::session::BulkPlan {
        verb: BulkVerb::Persist,
        eligible: vec![sid.clone()],
        skipped: Vec::new(),
    };
    match gait_ops::session::bulk_act(&gait_dir, &one, /* confirmed */ true) {
        Ok(_) => responder.respond(CloseSessionResponse::new()),
        Err(e) => responder.respond_with_internal_error(e.to_string()),
    }
}

/// `session/delete` — discard (drop `session/<id>` branch, no merge),
/// unrecoverable, no prompt (design §7). Symmetric with `close_session`.
pub fn delete_session(
    map: &SessionMap,
    req: DeleteSessionRequest,
    responder: Responder<DeleteSessionResponse>,
) -> Result<(), agent_client_protocol::Error> {
    let sid = req.session_id.0.to_string();
    let Some(gait_dir) = gait_dir_for_session(map, &sid) else {
        return responder.respond_with_internal_error(format!("unknown session: {sid}"));
    };

    map.remove(&sid); // cancel + join any live turn
    let plan = gait_ops::session::BulkPlan {
        verb: BulkVerb::Discard,
        eligible: vec![sid.clone()],
        skipped: Vec::new(),
    };
    match gait_ops::session::bulk_act(&gait_dir, &plan, true) {
        Ok(_) => responder.respond(DeleteSessionResponse::new()),
        Err(e) => responder.respond_with_internal_error(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gait_dir_for_session_resolves_via_map_first() {
        let map = SessionMap::new();
        map.insert(
            PathBuf::from("/g/.gait"),
            "sess-1".into(),
            PathBuf::from("/code"),
        );
        assert_eq!(
            gait_dir_for_session(&map, "sess-1"),
            Some(PathBuf::from("/g/.gait"))
        );
    }

    #[test]
    fn gait_dir_for_session_scans_known_gait_dirs_when_unminted() {
        // The session was minted by a different connection: not in `map`, but
        // its gait-dir was touched by *this* connection (another session on
        // the same workspace), and `ps` lists it there.
        let t = tempfile::TempDir::new().unwrap();
        let repo = git2::Repository::init(t.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "t").unwrap();
        cfg.set_str("user.email", "t@t.com").unwrap();
        let sig = git2::Signature::now("t", "t@t.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        drop(tree);
        gait_state::git::Repos::production()
            .traces
            .create_session_worktree(t.path(), "sess-2")
            .unwrap();
        let log_path = gait_state::session::live::log_path(t.path(), "sess-2");
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(&log_path, "").unwrap();

        let map = SessionMap::new();
        // A different session, same gait-dir, minted by this connection.
        map.insert(
            t.path().to_path_buf(),
            "sess-1".into(),
            t.path().to_path_buf(),
        );
        assert_eq!(
            gait_dir_for_session(&map, "sess-2"),
            Some(t.path().to_path_buf())
        );
    }

    #[test]
    fn gait_dir_for_session_none_when_nowhere_found() {
        let map = SessionMap::new();
        assert_eq!(gait_dir_for_session(&map, "nope"), None);
    }
}
