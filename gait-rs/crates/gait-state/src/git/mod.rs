pub mod checkpoint;
pub mod codebase;
pub mod trace;

use crate::git::checkpoint::{CheckpointRepo, GitCheckpointRepo};
use crate::git::codebase::{CodebaseRepo, GitCodebaseRepo};
use crate::git::trace::{GitTraceRepo, TraceRepo};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Appends the static gait attribution trailer. If `message` already ends in
/// a trailer block (i.e. already contains a blank-line paragraph break, as
/// produced by [`build_trace_message`]), the line is joined into that same
/// block with a single newline so the whole block stays contiguous for git
/// trailer tooling. Otherwise (a bare subject with no trailer block of its
/// own) a new blank-line paragraph is started, exactly as before.
pub fn with_gait_trailer(message: &str) -> String {
    let sep = if message.contains("\n\n") {
        "\n"
    } else {
        "\n\n"
    };
    format!("{message}{sep}Co-Authored-By: gait <gait@localhost>")
}

/// Structured trailers for a trace commit. Only populated (`Some`/non-empty)
/// fields are rendered as trailer lines — omitted fields produce no line
/// (never padded with the literal string `"none"`).
#[derive(Debug, Clone, Default)]
pub struct TraceTrailers {
    pub phase: String,
    pub turn: Option<u32>,
    pub parent_session: Option<String>,
    pub subagent_session: Option<String>,
    pub status: Option<String>,
    pub codebase: String,
    pub codebase_hash: String,
}

/// Builds `subject\n\n<trailer block>` — the trailer block does NOT include
/// the final `Co-Authored-By` line; callers pass the result of this function
/// as the `message` argument to `TraceRepo::commit_session`/`finish_session`,
/// which appends `Co-Authored-By` via [`with_gait_trailer`] internally.
pub fn build_trace_message(subject: &str, trailers: &TraceTrailers) -> String {
    let mut lines = vec![format!("Phase: {}", trailers.phase)];
    if let Some(turn) = trailers.turn {
        lines.push(format!("Turn: {turn}"));
    }
    if let Some(parent) = &trailers.parent_session {
        lines.push(format!("Parent-Session: {parent}"));
    }
    if let Some(sub) = &trailers.subagent_session {
        lines.push(format!("Subagent-Session: {sub}"));
    }
    if let Some(status) = &trailers.status {
        lines.push(format!("Status: {status}"));
    }
    lines.push(format!("Codebase: {}", trailers.codebase));
    lines.push(format!("Codebase-Hash: {}", trailers.codebase_hash));
    format!("{subject}\n\n{}", lines.join("\n"))
}

pub fn signature_or_default(repo: &git2::Repository) -> anyhow::Result<git2::Signature<'static>> {
    match repo.signature() {
        Ok(sig) => Ok(sig.to_owned()),
        Err(_) => Ok(git2::Signature::now("gait", "gait@localhost")?),
    }
}

pub fn checkpoint_git_dir(gait_dir: &Path, session_id: &str) -> PathBuf {
    crate::session::live::worktree_dir(gait_dir, session_id).join("codebase.git")
}

/// Bundle of the three git-context repos, injected from composition roots.
/// `run_turn` and tools receive a `Repos`; entry points build `Repos::production()`.
#[derive(Clone)]
pub struct Repos {
    pub codebase: Arc<dyn CodebaseRepo>,
    pub traces: Arc<dyn TraceRepo>,
    pub checkpoints: Arc<dyn CheckpointRepo>,
}

impl Repos {
    /// The real git-backed implementations used in production.
    pub fn production() -> Self {
        Repos {
            codebase: Arc::new(GitCodebaseRepo),
            traces: Arc::new(GitTraceRepo),
            checkpoints: Arc::new(GitCheckpointRepo),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_git_dir_is_under_the_active_mount() {
        let gait_dir = std::path::Path::new("/tmp/gd");
        assert_eq!(
            checkpoint_git_dir(gait_dir, "sabc123"),
            std::path::Path::new("/tmp/gd/.active/sabc123/codebase.git")
        );
    }

    #[test]
    fn with_gait_trailer_unchanged_for_a_bare_subject() {
        // Regression: existing callers (e.g. trace.rs's `finish_session`) pass a
        // bare one-line subject with no trailer block of their own. Must keep
        // producing a blank-line paragraph break before Co-Authored-By, exactly
        // as before this change — this is the literal string asserted at
        // `trace.rs`'s `commit_session_creates_commit` test.
        assert_eq!(
            with_gait_trailer("session(sabc1234): turn 1"),
            "session(sabc1234): turn 1\n\nCo-Authored-By: gait <gait@localhost>"
        );
    }

    #[test]
    fn with_gait_trailer_joins_an_existing_trailer_block_contiguously() {
        // When the message already ends in a trailer block (contains a blank
        // line already), Co-Authored-By must be appended as one more line in
        // that same block (single \n), not a new paragraph (\n\n) — otherwise
        // git tooling that reads the trailer block stops before Co-Authored-By.
        let msg = "session(sabc1234): turn 3\n\nPhase: turn\nTurn: 3";
        assert_eq!(
            with_gait_trailer(msg),
            "session(sabc1234): turn 3\n\nPhase: turn\nTurn: 3\nCo-Authored-By: gait <gait@localhost>"
        );
    }

    #[test]
    fn build_trace_message_turn_commit_has_no_optional_fields() {
        let trailers = TraceTrailers {
            phase: "turn".to_string(),
            turn: Some(3),
            parent_session: None,
            subagent_session: None,
            status: None,
            codebase: "/tmp/ws".to_string(),
            codebase_hash: "deadbeef".to_string(),
        };
        let msg = build_trace_message("session(sabc1234): turn 3", &trailers);
        assert_eq!(
            msg,
            "session(sabc1234): turn 3\n\nPhase: turn\nTurn: 3\nCodebase: /tmp/ws\nCodebase-Hash: deadbeef"
        );
    }

    #[test]
    fn build_trace_message_omits_absent_optional_fields() {
        // A `permissions`/`clarify`/etc. commit has no turn number, no
        // parent/subagent session, no status — none of those trailer lines
        // should appear, and none should be padded with "none".
        let trailers = TraceTrailers {
            phase: "permissions".to_string(),
            turn: None,
            parent_session: None,
            subagent_session: None,
            status: None,
            codebase: "/tmp/ws".to_string(),
            codebase_hash: "deadbeef".to_string(),
        };
        let msg = build_trace_message("session(sabc1234): permissions", &trailers);
        assert_eq!(
            msg,
            "session(sabc1234): permissions\n\nPhase: permissions\nCodebase: /tmp/ws\nCodebase-Hash: deadbeef"
        );
        assert!(!msg.contains("Turn:"));
        assert!(!msg.contains("none"));
    }

    #[test]
    fn build_trace_message_spawn_commit_has_parent_session() {
        let trailers = TraceTrailers {
            phase: "spawn".to_string(),
            turn: None,
            parent_session: Some("sabc1234".to_string()),
            subagent_session: None,
            status: None,
            codebase: "/tmp/ws".to_string(),
            codebase_hash: "deadbeef".to_string(),
        };
        let msg = build_trace_message("session(schild99): spawn from sabc1234", &trailers);
        assert_eq!(
            msg,
            "session(schild99): spawn from sabc1234\n\nPhase: spawn\nParent-Session: sabc1234\nCodebase: /tmp/ws\nCodebase-Hash: deadbeef"
        );
    }

    #[test]
    fn build_trace_message_completion_commit_has_subagent_session_and_status() {
        let trailers = TraceTrailers {
            phase: "completion".to_string(),
            turn: None,
            parent_session: None,
            subagent_session: Some("schild99".to_string()),
            status: Some("done".to_string()),
            codebase: String::new(),
            codebase_hash: String::new(),
        };
        let msg = build_trace_message("session(sabc1234): completion", &trailers);
        assert_eq!(
            msg,
            "session(sabc1234): completion\n\nPhase: completion\nSubagent-Session: schild99\nStatus: done\nCodebase: \nCodebase-Hash: "
        );
    }

    #[test]
    fn build_trace_message_then_with_gait_trailer_produces_one_contiguous_block() {
        let trailers = TraceTrailers {
            phase: "turn".to_string(),
            turn: Some(1),
            parent_session: None,
            subagent_session: None,
            status: None,
            codebase: "/tmp/ws".to_string(),
            codebase_hash: "deadbeef".to_string(),
        };
        let msg = build_trace_message("session(sabc1234): turn 1", &trailers);
        let full = with_gait_trailer(&msg);
        assert_eq!(
            full,
            "session(sabc1234): turn 1\n\nPhase: turn\nTurn: 1\nCodebase: /tmp/ws\nCodebase-Hash: deadbeef\nCo-Authored-By: gait <gait@localhost>"
        );
        // Exactly one blank line in the whole message (the subject/body
        // separator) — proves the trailer block plus Co-Authored-By stayed
        // contiguous, not split into two paragraphs.
        assert_eq!(full.matches("\n\n").count(), 1);
    }
}
