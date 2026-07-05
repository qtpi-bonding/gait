//! DiagnosticSink — single owner of writes to
//! `.active/<id>/sessions/<id>/diagnostic.jsonl`.
//!
//! Two disciplines, enforced in one place:
//!
//! - **`ping`** writes a per-phase Diagnostic frame (gated on `GAIT_DEBUG`).
//!   When the gate is off, the call is a no-op. This is the per-phase
//!   heartbeat the spine reads; turning it off in production keeps the
//!   `diagnostic.jsonl` stream minimal.
//!
//! - **`terminal`** writes a record for a terminal failure — ALWAYS, regardless
//!   of `GAIT_DEBUG`. A terminal failure must always be explained; the
//!   `gating test` (spine_relay_arms) pins the per-phase gate, and the
//!   `terminal-failure test` (spine_terminal_failure) pins the absence of a
//!   gate on terminal records.
//!
//! Best-effort: an IO error is logged once to stderr and dropped. Diagnostics
//! must never fail a run or interfere with `log.jsonl`.

use std::path::{Path, PathBuf};

/// Sole owner of `diagnostic.jsonl` writes for one session.
///
/// Build once near the top of `relay_brain_turn`, after `TurnCtx`, and pass
/// `&DiagnosticSink` to the three sites that previously called
/// `write_diagnostic_line` directly. Cheap to construct — just a path
/// clone and a session-id string.
pub(crate) struct DiagnosticSink {
    path: PathBuf,
    session_id: String,
}

impl DiagnosticSink {
    /// Build the sink for a given session. Resolves the canonical
    /// `.active/<id>/sessions/<id>/diagnostic.jsonl` path (live nested shape,
    /// matching the persisted tree shape — see `crate::session::live`);
    /// the directory is created lazily on first write (no need to mkdir here).
    pub(crate) fn new(gait_dir: &Path, session_id: &str) -> Self {
        Self {
            path: crate::session::live::diagnostic_path(gait_dir, session_id),
            session_id: session_id.to_string(),
        }
    }

    /// Per-phase diagnostic ping. Gated on `GAIT_DEBUG` — when unset, this is
    /// a no-op. Use for the spine's `Signal::Body::Diagnostic` arm.
    pub(crate) fn ping(&self, record_json: &str) {
        if !crate::diagnostic::debug_enabled() {
            return;
        }
        self.write_line(record_json);
    }

    /// Terminal-failure record. ALWAYS written, regardless of `GAIT_DEBUG`.
    /// Use for `emit_terminal_failure` (watchdog breach) and the failed
    /// `Terminated` arm.
    pub(crate) fn terminal(&self, record_json: &str) {
        self.write_line(record_json);
    }

    /// Borrow the session id baked into the sink. `emit_terminal_failure`
    /// needs this to populate the `session` field of the terminal DiagRecord;
    /// we thread it through the sink rather than re-passing the string at
    /// every call site.
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    fn write_line(&self, line: &str) {
        use std::io::Write;
        // The nested sessions/<id>/ directory (see crate::session::live) may
        // not exist yet on a brand new session — create it lazily here so a
        // terminal-failure record (which must ALWAYS be written) doesn't
        // silently drop because its parent directory was never created.
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = res {
            #[allow(clippy::print_stderr)]
            // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
            {
                eprintln!("[gait] diagnostic write failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-phase pings are GAIT_DEBUG-gated: with the env var unset, no
    /// `diagnostic.jsonl` is created. Mirrors the integration pin in
    /// `spine_relay_arms::diagnostic_writes_diagnostic_jsonl_only_when_gait_debug_set`.
    #[test]
    fn ping_is_suppressed_when_gait_debug_unset() {
        // Make sure GAIT_DEBUG is unset for this test, regardless of the
        // surrounding shell environment.
        std::env::remove_var("GAIT_DEBUG");
        assert!(
            !crate::diagnostic::debug_enabled(),
            "precondition: GAIT_DEBUG must be unset"
        );

        let tmp = tempfile::tempdir().unwrap();
        let session = "diag_ping_off";
        let path = crate::session::live::diagnostic_path(tmp.path(), session);
        let sink = DiagnosticSink::new(tmp.path(), session);

        sink.ping(r#"{"phase":"awaiting_headers"}"#);
        sink.ping(r#"{"phase":"streaming"}"#);
        assert!(
            !path.exists(),
            "diagnostic.jsonl must NOT be created when GAIT_DEBUG is unset; \
             a refactor that drops the gate will fail this test."
        );
    }

    /// Terminal records are ALWAYS written — `GAIT_DEBUG` is irrelevant.
    /// Mirrors the integration pin in
    /// `spine_terminal_failure::failed_terminated_sets_error_and_writes_diagnostic`.
    #[test]
    fn terminal_always_writes_even_without_gait_debug() {
        std::env::remove_var("GAIT_DEBUG");
        assert!(
            !crate::diagnostic::debug_enabled(),
            "precondition: GAIT_DEBUG must be unset"
        );

        let tmp = tempfile::tempdir().unwrap();
        let session = "diag_terminal";
        let path = crate::session::live::diagnostic_path(tmp.path(), session);
        let sink = DiagnosticSink::new(tmp.path(), session);

        let rec = crate::diagnostic::DiagRecord::terminal(
            session,
            crate::diagnostic::Phase::Streaming,
            "brain reported failure via Terminated signal",
        );
        sink.terminal(&rec.to_json_line());
        assert!(
            path.exists(),
            "terminal record MUST be written even when GAIT_DEBUG is unset"
        );
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 1, "exactly one terminal line");
        assert!(
            body.contains(r#""event":"error""#),
            "terminal record carries the error event tag: {body}"
        );
        assert!(
            body.contains("brain reported failure via Terminated signal"),
            "terminal record carries the failure detail: {body}"
        );
    }
}
