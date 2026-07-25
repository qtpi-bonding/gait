//! Per-step completion poll for the in-flight turn.
//!
//! The turn loop calls into this module at the top of each step, just before
//! building the next LLM request. The job is narrow:
//!
//! * Compute which daemon completions and child-agent completions are ready to
//!   surface to the parent right now.
//! * Filter out completions that are already represented by a `*_update`
//!   event in the parent log (the dedup ledger).
//! * Hand the canonical `Completion` values back to the loop, which records the
//!   event via `emitter.record(c.body())`. No `messages.push(...)` — the
//!   LLM's request is rebuilt from `coalesce(retrace(log))`, so writing the
//!   event is the only injection that matters.
//!
//! The helper is split out of the turn loop so the impure log/registry reads
//! stay behind pure-ish functions in this module, which keeps the loop body
//! flat and makes the dedup scan unit-testable.

use std::collections::HashSet;
use std::path::Path;

use crate::proto::event_kind;
use crate::proto::gait::log::v1::payload;
use crate::proto::gait::log::v1::Event;
use crate::session::completion::Completion;

/// Idempotency keys (see `Completion`, `gait-state::session::completion`) for
/// daemons that already have a `DaemonUpdate` event in this session's log.
///
/// Used as the dedup ledger: the per-step poll reads this set and a completion
/// whose idempotency_key is in the set has already been surfaced in a prior
/// step or by the producer's `notify_parent` call, so it must NOT be
/// re-injected. Renamed from `surfaced_daemon_names` — the old name implied
/// (and the old implementation enforced) that a daemon's NAME alone was
/// sufficient identity, which is wrong: two separate runs of a same-named
/// daemon are two distinct completions, not duplicates of each other.
pub fn surfaced_daemon_idempotency_keys(events: &[Event]) -> HashSet<String> {
    let mut out = HashSet::new();
    for ev in events {
        if let Some(payload::Kind::DaemonUpdate(du)) = event_kind(ev) {
            out.insert(du.idempotency_key.clone());
        }
    }
    out
}

/// Daemons owned by `session_id` that have finished AND are not yet surfaced.
///
/// Two finish sources are merged:
///
/// 1. **Explicit Stop records** in the registry (`stopped_for_session`) carry
///    the recorded exit code. This covers the normal exit path and timeouts
///    (exit_code = 124). The agent sees `status = "done"` or `"killed"`.
///
/// 2. **Killed-without-Stop** (SIGKILL, OOM, reboot): the daemon Start is
///    still in the registry and `!is_alive()`. The agent sees
///    `status = "killed"` with `exit_code = -9`. Without this branch a
///    daemon that died ungracefully would never be surfaced.
pub fn pending_daemon_completions(
    gait_dir: &Path,
    session_id: &str,
    already: &HashSet<String>,
) -> Vec<Completion> {
    let mut out = Vec::new();
    // 1. Explicit stops — exit code is the recorded one.
    for (name, exit_code) in crate::session::daemons::stopped_for_session(gait_dir, session_id) {
        if already.contains(&format!("{name}@{}", started_at_for(gait_dir, &name))) {
            continue;
        }
        // exit_code on the event still lets the agent reason precisely; the
        // status text is the shared done/killed classification (see
        // `daemons::classify_exit`).
        let status = crate::session::daemons::classify_exit(exit_code);
        let log_path = gait_dir
            .join("daemons")
            .join(session_id)
            .join(format!("{name}.log"))
            .to_string_lossy()
            .into_owned();
        let started_at = started_at_for(gait_dir, &name);
        let idempotency_key = format!("{name}@{started_at}");
        out.push(Completion {
            provenance: None,
            payload: crate::proto::events::daemon_update(
                name,
                exit_code,
                status,
                log_path,
                idempotency_key.clone(),
            ),
            idempotency_key,
        });
    }
    // 2. Killed-without-Stop — the Start is in `active()` but the process is
    // no longer alive. Synthetic completion carries exit_code = -9.
    if let Ok(active) = crate::session::daemons::active(gait_dir) {
        for rec in active {
            if rec.session != session_id {
                continue;
            }
            if already.contains(&format!("{}@{}", rec.name, rec.started_at)) {
                continue;
            }
            if !crate::session::daemons::is_alive(&rec) {
                let idempotency_key = format!("{}@{}", rec.name, rec.started_at);
                out.push(Completion {
                    provenance: None,
                    payload: crate::proto::events::daemon_update(
                        rec.name.clone(),
                        -9,
                        crate::session::daemons::classify_exit(-9),
                        rec.log_path.clone(),
                        idempotency_key.clone(),
                    ),
                    idempotency_key,
                });
            }
        }
    }
    out
}

/// Look up a daemon's `started_at` from the on-disk registry. Falls back to
/// 0 if the registry is unreadable or the daemon is not present (the latter
/// shouldn't happen for a `stopped_for_session` entry, but matching the
/// previous behavior keeps tests passing).
fn started_at_for(gait_dir: &Path, name: &str) -> u64 {
    crate::session::daemons::history(gait_dir)
        .ok()
        .and_then(|h| h.into_iter().find(|(rec, _)| rec.name == name))
        .map(|(rec, _)| rec.started_at)
        .unwrap_or(0)
}

/// Idempotency keys (see `Completion`) for child agents that already have an
/// `AgentUpdate` event in this session's log.
///
/// Symmetric to `surfaced_daemon_idempotency_keys`: the per-step poll reads
/// this set and a completion whose idempotency_key is in the set has already
/// been surfaced, so it must NOT be re-injected. Renamed from
/// `surfaced_agent_ids` for the same reason as the daemon version — child
/// `session_id` alone is not stable identity.
pub fn surfaced_agent_idempotency_keys(events: &[Event]) -> HashSet<String> {
    let mut out = HashSet::new();
    for ev in events {
        if let Some(payload::Kind::AgentUpdate(au)) = event_kind(ev) {
            out.insert(au.idempotency_key.clone());
        }
    }
    out
}

/// Child sessions of `session_id` that have finished AND are not yet surfaced.
///
/// "Finished" = the child's session lock is absent or stale (dead PID). The
/// summary comes from the child's last `AssistantText` event.
pub fn pending_agent_completions(
    gait_dir: &Path,
    session_id: &str,
    already: &HashSet<String>,
) -> Vec<Completion> {
    let mut out = Vec::new();
    for child in crate::session::children_of(gait_dir, session_id) {
        // "Already surfaced" = the completion's idempotency_key (the child's
        // own log-tail event id at detection time) is in the ledger set
        // returned by `surfaced_agent_idempotency_keys`.
        if already.contains(&child.last_event_id) {
            continue;
        }
        if child.is_done {
            let idempotency_key = child.last_event_id.clone();
            out.push(Completion {
                provenance: Some(crate::proto::gait::log::v1::Provenance {
                    from: format!("agent:{}", child.id).into_bytes(),
                    to: vec![],
                    context: String::new(),
                    signature: vec![], // placeholder identity — see plan Global Constraints
                }),
                payload: crate::proto::events::agent_update(
                    child.id.clone(),
                    "done",
                    child.summary.unwrap_or_default(),
                    None,
                    idempotency_key.clone(),
                ),
                idempotency_key,
            });
        }
    }
    out
}

// =============================================================================
// tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::envelope::mint;

    /// Wrap a `Body` in a fresh `Event` (seq is irrelevant for these scans).
    fn ev(body: crate::proto::gait::log::v1::event::Body) -> Event {
        mint(body, None, 0, 0)
    }

    /// Extract the payload kind out of a `Completion` for test assertions.
    fn completion_kind(c: &Completion) -> payload::Kind {
        match &c.payload {
            crate::proto::gait::log::v1::event::Body::Payload(p) => p
                .kind
                .clone()
                .expect("Completion payload must carry a kind"),
            _ => panic!("Completion payload must be a Payload body"),
        }
    }

    #[test]
    fn surfaced_names_collects_daemon_updates() {
        let events = vec![
            ev(crate::proto::events::daemon_update(
                "a".into(),
                0,
                "done",
                "/l".into(),
                "a@1".into(),
            )),
            ev(crate::proto::events::daemon_update(
                "b".into(),
                1,
                "done",
                "/l".into(),
                "b@1".into(),
            )),
        ];
        let s = surfaced_daemon_idempotency_keys(&events);
        assert!(s.contains("a@1") && s.contains("b@1") && s.len() == 2);
    }

    #[test]
    fn surfaced_names_ignores_non_daemon_events() {
        let events = vec![
            ev(crate::proto::events::assistant_text("hi".into())),
            ev(crate::proto::events::user_text("hello".into(), None)),
        ];
        let s = surfaced_daemon_idempotency_keys(&events);
        assert!(s.is_empty(), "no DaemonUpdate events → empty set");
    }

    #[test]
    fn surfaced_daemon_names_distinguishes_repeated_runs_by_idempotency_key() {
        // Two distinct runs of a same-named daemon ("build") have different
        // idempotency keys (one per started_at); the surfaced set must key
        // on the idempotency_key, NOT on the daemon name, so the second run
        // is not silently swallowed as a duplicate of the first.
        let events = vec![ev(crate::proto::events::daemon_update(
            "build".into(),
            0,
            "done".into(),
            "/l".into(),
            "build@111".into(),
        ))];
        let surfaced = surfaced_daemon_idempotency_keys(&events);
        assert!(surfaced.contains("build@111"));
        assert!(
            !surfaced.contains("build@222"),
            "a different run (different started_at) must not be considered surfaced"
        );
    }
    #[test]
    fn pending_excludes_already_surfaced_and_live() {
        use crate::proto::gait::log::v1 as pb;
        use crate::session::daemons;
        use std::process::Command;

        let dir = tempfile::TempDir::new().unwrap();
        let gait_dir = dir.path();

        // Daemon "done1" explicitly stopped (owned by "S", exit code 0).
        // We need a real Start on disk so stopped_for_session can match the
        // owning-session field back.
        let start1 = pb::DaemonStart {
            name: "done1".into(),
            pid: 555_001,
            pgid: 555_001,
            started_at: 1,
            cmd: "echo done1".into(),
            cwd: "/tmp".into(),
            log_path: "/tmp/done1.log".into(),
            session: "S".into(),
        };
        daemons::append_start(gait_dir, &start1).unwrap();
        daemons::append_stop(gait_dir, "done1", 0).unwrap();

        // Daemon "live1" alive (owned by "S"). The poll must NOT surface it.
        let mut child = unsafe {
            use std::os::unix::process::CommandExt;
            Command::new("sleep")
                .arg("999")
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()
                .expect("spawn sleep 999")
        };
        let pid = child.id() as i32;
        let pgid = unsafe { libc::getpgid(pid) };
        assert!(pgid > 0);
        let started_at = crate::process::proc_start_time(pid).expect("proc_start_time");
        let start2 = pb::DaemonStart {
            name: "live1".into(),
            pid,
            pgid,
            started_at,
            cmd: "sleep 999".into(),
            cwd: "/tmp".into(),
            log_path: "/tmp/live1.log".into(),
            session: "S".into(),
        };
        daemons::append_start(gait_dir, &start2).unwrap();

        // Daemon "other" explicitly stopped (owned by a DIFFERENT session) —
        // the poll must NOT surface it.
        let start3 = pb::DaemonStart {
            name: "other".into(),
            pid: 555_003,
            pgid: 555_003,
            started_at: 1,
            cmd: "echo other".into(),
            cwd: "/tmp".into(),
            log_path: "/tmp/other.log".into(),
            session: "T".into(),
        };
        daemons::append_start(gait_dir, &start3).unwrap();
        daemons::append_stop(gait_dir, "other", 0).unwrap();

        // `already` says "done0" was surfaced previously; must not affect the
        // current surface (different name).
        let mut already = HashSet::new();
        already.insert("done0".to_string());

        let pending = pending_daemon_completions(gait_dir, "S", &already);

        let names: HashSet<String> = pending
            .iter()
            .map(|c| match completion_kind(c) {
                payload::Kind::DaemonUpdate(du) => du.name,
                _ => unreachable!(),
            })
            .collect();
        assert!(
            names.contains("done1"),
            "stopped+session-owned daemon must be surfaced: {names:?}"
        );
        assert!(
            !names.contains("live1"),
            "still-alive daemon must not be surfaced: {names:?}"
        );
        assert!(
            !names.contains("other"),
            "other-session daemon must not be surfaced: {names:?}"
        );
        assert_eq!(names.len(), 1, "exactly one pending completion: {names:?}");

        // Surface the entry: exit_code must be the recorded 0, status "done".
        for c in &pending {
            if let payload::Kind::DaemonUpdate(du) = completion_kind(c) {
                assert_eq!(du.name, "done1");
                assert_eq!(du.exit_code, 0, "done1 exit_code = 0");
                assert_eq!(
                    crate::proto::gait::log::v1::DaemonStatus::try_from(du.status).unwrap(),
                    crate::proto::gait::log::v1::DaemonStatus::Done
                );
                assert!(
                    du.log_path.ends_with("done1.log"),
                    "log_path under daemons/: {}",
                    du.log_path
                );
                assert!(
                    du.log_path.contains("daemons/S/"),
                    "log_path must be namespaced under the owning session's \
                     own daemons/<session_id>/ dir, got: {}",
                    du.log_path
                );
            }
        }

        // Cleanup the live child.
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn pending_includes_killed_without_stop() {
        use crate::proto::gait::log::v1 as pb;
        use crate::session::daemons;

        let dir = tempfile::TempDir::new().unwrap();
        let gait_dir = dir.path();

        // Register a Start whose pid points at a real but transient process:
        // spawn sleep, immediately wait() it (no Stop appended), then poll.
        let pid = 1_i32; // pid 1 is alive but identity (start-time) never matches
        let start = pb::DaemonStart {
            name: "dead-killed".into(),
            pid,
            pgid: pid,
            started_at: u64::MAX, // forces is_alive() to false
            cmd: "init".into(),
            cwd: "/tmp".into(),
            log_path: "/tmp/dead-killed.log".into(),
            session: "S".into(),
        };
        daemons::append_start(gait_dir, &start).unwrap();

        // already starts empty — the new completion must be surfaced.
        let already = HashSet::new();
        let pending = pending_daemon_completions(gait_dir, "S", &already);

        let mut names: Vec<String> = pending
            .iter()
            .map(|c| match completion_kind(c) {
                payload::Kind::DaemonUpdate(du) => du.name,
                _ => unreachable!(),
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["dead-killed".to_string()], "got: {names:?}");

        for c in &pending {
            if let payload::Kind::DaemonUpdate(du) = completion_kind(c) {
                assert_eq!(du.name, "dead-killed");
                assert_eq!(du.exit_code, -9, "killed-no-stop uses exit_code = -9");
                assert_eq!(
                    crate::proto::gait::log::v1::DaemonStatus::try_from(du.status).unwrap(),
                    crate::proto::gait::log::v1::DaemonStatus::Killed
                );
            }
        }

        // Dedup: same registry, name already surfaced → empty.
        let mut already_seen = HashSet::new();
        // The surfaced set now keys on the daemon's idempotency_key
        // (`format!("{name}@{started_at}")`), not the bare name. Mirror
        // that here so a repeat surface is suppressed.
        already_seen.insert(format!("dead-killed@{}", u64::MAX));
        let pending2 = pending_daemon_completions(gait_dir, "S", &already_seen);
        assert!(
            pending2.is_empty(),
            "already-surfaced killed-no-stop must be deduped"
        );
    }

    #[test]
    fn pending_empty_for_unknown_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let already = HashSet::new();
        assert!(pending_daemon_completions(dir.path(), "NoSuchSession", &already).is_empty());
    }

    // ── Agent completion tests ────────────────────────────────────────────────

    #[test]
    fn surfaced_agent_ids_collects_agent_updates() {
        let events = vec![ev(crate::proto::events::agent_update(
            "schild01".into(),
            "done",
            "finished fine".into(),
            None,
            "schild01-evt1".into(),
        ))];
        let s = surfaced_agent_idempotency_keys(&events);
        assert!(
            s.contains("schild01-evt1"),
            "schild01-evt1 must be in surfaced set"
        );
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn surfaced_agent_ids_ignores_non_agent_events() {
        let events = vec![
            ev(crate::proto::events::assistant_text("hi".into())),
            ev(crate::proto::events::user_text("hello".into(), None)),
        ];
        let s = surfaced_agent_idempotency_keys(&events);
        assert!(s.is_empty(), "no AgentUpdate events → empty set");
    }

    #[test]
    fn pending_agent_excludes_already_surfaced_and_running() {
        use crate::session::log::{init_session, InitSessionOpts};

        let dir = tempfile::TempDir::new().unwrap();
        let gait_dir = dir.path();

        // Child "schild02" done (no lock file), owned by "sparent".
        let log_path = crate::session::live::log_path(gait_dir, "schild02");
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        init_session(InitSessionOpts {
            log_path,
            id: "schild02".to_string(),
            model: String::new(),
            codebase_hash: String::new(),
            codebase_dirty: false,
            assigned_by: Some("sparent".to_string()),
            codebase_path: None,
            notify_done: false,
        })
        .unwrap();
        let schild02_last_event_id =
            crate::session::log::read_events(&crate::session::live::log_path(gait_dir, "schild02"))
                .unwrap()
                .last()
                .map(|e| e.id.clone())
                .unwrap_or_default();

        // Child "schild03" still running — write lock file with OWN pid so is_alive check passes.
        let child03_dir = crate::session::live::worktree_dir(gait_dir, "schild03");
        std::fs::create_dir_all(&child03_dir).unwrap();
        let log_path3 = crate::session::live::log_path(gait_dir, "schild03");
        std::fs::create_dir_all(log_path3.parent().unwrap()).unwrap();
        init_session(InitSessionOpts {
            log_path: log_path3,
            id: "schild03".to_string(),
            model: String::new(),
            codebase_hash: String::new(),
            codebase_dirty: false,
            assigned_by: Some("sparent".to_string()),
            codebase_path: None,
            notify_done: false,
        })
        .unwrap();
        // Write our own pid as the lock — so kill(pid, 0) succeeds and is_done = false.
        let lock_path3 = child03_dir.join("lock");
        std::fs::write(&lock_path3, std::process::id().to_string()).unwrap();

        let already = HashSet::new();
        let pending = pending_agent_completions(gait_dir, "sparent", &already);
        let ids: Vec<String> = pending
            .iter()
            .map(|c| match completion_kind(c) {
                payload::Kind::AgentUpdate(au) => au.session_id,
                _ => unreachable!(),
            })
            .collect();
        assert!(
            ids.contains(&"schild02".to_string()),
            "done child must be surfaced: {ids:?}"
        );
        assert!(
            !ids.contains(&"schild03".to_string()),
            "running child must NOT be surfaced: {ids:?}"
        );

        // Dedup: schild02 already surfaced → empty.
        let mut already_seen = HashSet::new();
        // The surfaced set now keys on the child's `last_event_id` (the
        // idempotency_key stamped on its AgentUpdate). Populate `already`
        // with schild02's tail event id so it suppresses a repeat surface.
        already_seen.insert(schild02_last_event_id.clone());
        let pending2 = pending_agent_completions(gait_dir, "sparent", &already_seen);
        assert!(
            pending2.is_empty(),
            "already-surfaced agent must be deduped: {pending2:?}"
        );
    }
}
