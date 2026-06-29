//! Per-step completion poll for the in-flight turn.
//!
//! The turn loop calls into this module at the top of each step, just before
//! building the next LLM request. The job is narrow:
//!
//! * Compute which daemon completions (and later child-agent completions) are
//!   ready to surface to the parent right now.
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

/// Daemon names that already have a `DaemonUpdate` event in this session's log.
///
/// Used as the dedup ledger: the per-step poll reads this set and a completion
/// whose daemon name is in the set has already been surfaced in a prior step
/// or by the producer's `notify_parent` call, so it must NOT be re-injected.
pub fn surfaced_daemon_names(events: &[Event]) -> HashSet<String> {
    let mut out = HashSet::new();
    for ev in events {
        if let Some(payload::Kind::DaemonUpdate(du)) = event_kind(ev) {
            out.insert(du.name.clone());
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
        if already.contains(&name) {
            continue;
        }
        // 124 is the timeout exit code from cmd_daemons_stop; negative codes
        // are signal-terminated. Treat both as "killed" for the surfaced text;
        // exit_code on the event still lets the agent reason precisely.
        let status = if exit_code == 124 || exit_code < 0 {
            "killed"
        } else {
            "done"
        };
        let log_path = gait_dir
            .join("daemons")
            .join(format!("{name}.log"))
            .to_string_lossy()
            .into_owned();
        out.push(Completion::Daemon {
            name,
            exit_code,
            status: status.into(),
            log_path,
        });
    }
    // 2. Killed-without-Stop — the Start is in `active()` but the process is
    // no longer alive. Synthetic completion carries exit_code = -9.
    if let Ok(active) = crate::session::daemons::active(gait_dir) {
        for rec in active {
            if rec.session != session_id {
                continue;
            }
            if already.contains(&rec.name) {
                continue;
            }
            if !crate::session::daemons::is_alive(&rec) {
                out.push(Completion::Daemon {
                    name: rec.name.clone(),
                    exit_code: -9,
                    status: "killed".into(),
                    log_path: rec.log_path.clone(),
                });
            }
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

    #[test]
    fn surfaced_names_collects_daemon_updates() {
        let events = vec![
            ev(crate::proto::events::daemon_update(
                "a".into(),
                0,
                "done",
                "/l".into(),
            )),
            ev(crate::proto::events::daemon_update(
                "b".into(),
                1,
                "done",
                "/l".into(),
            )),
        ];
        let s = surfaced_daemon_names(&events);
        assert!(s.contains("a") && s.contains("b") && s.len() == 2);
    }

    #[test]
    fn surfaced_names_ignores_non_daemon_events() {
        let events = vec![
            ev(crate::proto::events::assistant_text("hi".into())),
            ev(crate::proto::events::user_text("hello".into(), None)),
        ];
        let s = surfaced_daemon_names(&events);
        assert!(s.is_empty(), "no DaemonUpdate events → empty set");
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
        let started_at = daemons::proc_start_time(pid).expect("proc_start_time");
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
            .map(|c| match c {
                Completion::Daemon { name, .. } => name.clone(),
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
            if let Completion::Daemon {
                name,
                exit_code,
                status,
                log_path,
            } = c
            {
                assert_eq!(name, "done1");
                assert_eq!(*exit_code, 0, "done1 exit_code = 0");
                assert_eq!(status, "done");
                assert!(
                    log_path.ends_with("done1.log"),
                    "log_path under daemons/: {log_path}"
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
            .map(|c| match c {
                Completion::Daemon { name, .. } => name.clone(),
                _ => unreachable!(),
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["dead-killed".to_string()], "got: {names:?}");

        for c in &pending {
            if let Completion::Daemon {
                name,
                exit_code,
                status,
                ..
            } = c
            {
                assert_eq!(name, "dead-killed");
                assert_eq!(*exit_code, -9, "killed-no-stop uses exit_code = -9");
                assert_eq!(status, "killed");
            }
        }

        // Dedup: same registry, name already surfaced → empty.
        let mut already_seen = HashSet::new();
        already_seen.insert("dead-killed".to_string());
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
}
