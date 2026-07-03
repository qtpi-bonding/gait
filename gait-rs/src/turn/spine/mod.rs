//! The turn spine: the relay between a brain subprocess and the tool handlers,
//! and the system's only unsealed writer (the security boundary).
//!
//! - `relay`     — the signal loop + RelayState (owns all loop state).
//! - `watchdog`  — liveness (ping grace, phase caps, absolute backstop).
//! - `handlers`  — business logic per signal (tool exec, subagent spawn).
//! - `preview`   — spine-side permission/stream enrichment (brain can't read disk).
//! - `lifecycle` — profile build, brain spawn, and the load-bearing teardown().
//!
//! Teardown ordering is fixed in `lifecycle::teardown`: drain in-flight tasks
//! before reaping the cascade group; both before checkpoint/commit.

mod watchdog;
#[allow(unused_imports)] // path-stable facade re-export; no in-crate caller yet
pub(crate) use watchdog::{watchdog_breach, SpinePhase, Watchdog, WatchdogCfg};

mod preview;
#[allow(unused_imports)] // path-stable facade re-export; no in-crate caller yet
pub(crate) use preview::enrich_permissions_preview;
mod util;
pub use util::strip_task_from_args;
#[allow(unused_imports)] // path-stable facade re-export; no in-crate caller yet
pub(crate) use util::turn_result_from_json;

mod lifecycle;
#[allow(unused_imports)] // path-stable facade re-export; no in-crate caller yet
pub(crate) use lifecycle::brain_argv;
#[allow(unused_imports)]
// build_turn_profile: path-stable facade re-export; no in-crate caller yet
pub use lifecycle::{build_turn_profile, ProfileInputs, TurnMode};

mod handlers;
#[allow(unused_imports)] // path-stable facade re-export; no in-crate caller yet
pub(crate) use handlers::tool::run_tool_as_spine;
#[allow(unused_imports)] // path-stable facade re-export; no in-crate caller yet
pub(crate) use handlers::{error_feedback, TurnCtx};

mod relay;
pub use relay::{relay_brain_turn, SignalSlot};

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[test]
    fn diagnostic_line_is_appended_to_diagnostic_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path();
        let session = "stest";
        std::fs::create_dir_all(gait_dir.join("sessions").join(session)).unwrap();

        crate::turn::DiagnosticSink::new(gait_dir, session)
            .terminal(r#"{"phase":"awaiting_headers"}"#);
        crate::turn::DiagnosticSink::new(gait_dir, session).terminal(r#"{"phase":"streaming"}"#);

        let p = gait_dir
            .join("sessions")
            .join(session)
            .join("diagnostic.jsonl");
        let body = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("awaiting_headers"));
        // And it did NOT touch log.jsonl
        assert!(!gait_dir
            .join("sessions")
            .join(session)
            .join("log.jsonl")
            .exists());
    }
}
