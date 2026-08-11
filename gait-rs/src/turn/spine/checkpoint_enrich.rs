//! Spine-side checkpoint enrichment: stamps a shadow-git SHA onto ToolResult
//! events (direct or Replace-wrapped) produced by shell/write/apply_patch,
//! since only the spine has a SessionGit handle — the brain is sealed.
//!
//! Proto nesting note: per `proto/log.proto`, `Event.body` is a top-level
//! `oneof` with `Payload payload = 16; Redact redact = 17; Replace replace = 18;`.
//! So a `Replace` is `event::Body::Replace(replace)` — not a variant of
//! `payload::Kind`. The Replace's `replacement` is itself a `Payload`, whose
//! `kind` is the inner `oneof` containing `ToolResult`. This file reflects
//! that exact shape.

use crate::proto::gait::log::v1::{event, payload, Event};
use crate::turn::SessionGit;
use std::path::Path;

const CHECKPOINT_GATED_TOOLS: [&str; 3] = ["shell", "write", "apply_patch"];

/// `tool_name_for_call(call_id)` must return the originating tool's name for a
/// given call_id, or `None` if it isn't a gated tool / isn't known. The caller
/// supplies this from `RelayState.pending_calls` (for a same-event ToolResult,
/// looked up before it's removed) or `RelayState.completed_mutations` (for a
/// later Replace of that same call_id).
pub(crate) fn enrich_checkpoint(
    event: &mut Event,
    tool_name_for_call: impl Fn(&str) -> Option<String>,
    git: &SessionGit,
    codebase: &Path,
    turn_n: u32,
) {
    // `payload::Kind::ToolResult(tr)` for a direct event; and `Replace(...)`
    // when a later event rewrites a prior `ToolResult` — in that case the
    // replacement's `kind` is the inner `ToolResult` (proto comment confirms
    // a Replace is always fully-typed).
    let tr = match event.body.as_mut() {
        Some(event::Body::Payload(p)) => match p.kind.as_mut() {
            Some(payload::Kind::ToolResult(tr)) => tr,
            _ => return,
        },
        Some(event::Body::Replace(replace)) => {
            let Some(replacement) = replace.replacement.as_mut() else {
                return;
            };
            match replacement.kind.as_mut() {
                Some(payload::Kind::ToolResult(tr)) => tr,
                _ => return,
            }
        }
        _ => return,
    };

    let Some(name) = tool_name_for_call(&tr.call_id) else {
        return;
    };
    if !CHECKPOINT_GATED_TOOLS.contains(&name.as_str()) {
        return;
    }

    if let Ok(sha) = git.checkpoint(codebase, turn_n) {
        if !sha.is_empty() {
            tr.checkpoint = Some(sha);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{checkpoint::CheckpointRepo, codebase::CodebaseRepo, trace::TraceRepo, Repos};
    use crate::proto::gait::log::v1::{event, payload, Payload, Replace, ToolResult};
    use std::sync::{Arc, Mutex};

    /// Mirrors the `Rec` fake in `crate::turn::session_git`'s own tests:
    /// implements both `TraceRepo` and `CheckpointRepo` so a single value
    /// can be used to build a `Repos` (which needs all three trait objects).
    /// `checkpoint()` returns a fixed non-empty SHA — the "stamps" tests
    /// below assert that exact SHA was forwarded onto the enriched
    /// `ToolResult.checkpoint`.
    #[derive(Clone, Default)]
    struct FakeRec(Arc<Mutex<Vec<String>>>);

    impl TraceRepo for FakeRec {
        fn init_workspace(&self, _gait_dir: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_session_worktree(&self, _gait_dir: &Path, _s: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn commit_session(&self, _gait_dir: &Path, _s: &str, _message: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn list_active_sessions(&self, _gait_dir: &Path) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn finish_session(&self, _gait_dir: &Path, _s: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_branch_worktree(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: &Path,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn remove_branch_worktree(&self, _: &Path, _: &Path) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn merge_branch_ff(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn discard_session(&self, _gait_dir: &Path, _session_id: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    impl CheckpointRepo for FakeRec {
        fn init(&self, _: &Path, _s: &str, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn checkpoint(&self, _: &Path, _s: &str, _: &Path, _t: u32) -> anyhow::Result<String> {
            self.0.lock().unwrap().push("checkpoint".into());
            Ok("deadbeef".to_string())
        }
        fn restore_to(&self, _: &Path, _: &str, _: &Path, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn remove(&self, _: &Path, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn diff_between(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: &str,
        ) -> anyhow::Result<Option<crate::proto::gait::log::v1::ChangeSummary>> {
            unimplemented!()
        }
    }

    struct NoCb;
    impl CodebaseRepo for NoCb {
        fn head_hash(&self, _: &Path) -> Option<String> {
            None
        }
        fn dirty_files(&self, _: &Path) -> Vec<String> {
            vec![]
        }
    }

    fn build_session_git() -> (Arc<FakeRec>, SessionGit) {
        let rec = Arc::new(FakeRec::default());
        let repos = Repos {
            codebase: Arc::new(NoCb),
            traces: rec.clone(),
            checkpoints: rec.clone(),
        };
        let git = SessionGit::for_session(&repos, Path::new("/tmp/gd"), "sess");
        (rec, git)
    }

    fn tool_result_event(call_id: &str) -> Event {
        Event {
            body: Some(event::Body::Payload(Payload {
                kind: Some(payload::Kind::ToolResult(ToolResult {
                    call_id: call_id.to_string(),
                    ..Default::default()
                })),
            })),
            ..Default::default()
        }
    }

    #[test]
    fn stamps_checkpoint_on_toolresult_for_gated_tool() {
        let (rec, git) = build_session_git();
        let mut ev = tool_result_event("call-1");
        enrich_checkpoint(
            &mut ev,
            |cid| (cid == "call-1").then(|| "shell".to_string()),
            &git,
            Path::new("/tmp/cb"),
            1,
        );
        let Some(event::Body::Payload(p)) = ev.body else {
            panic!("expected payload body");
        };
        let Some(payload::Kind::ToolResult(tr)) = p.kind else {
            panic!("expected ToolResult kind");
        };
        assert_eq!(tr.checkpoint.as_deref(), Some("deadbeef"));
        assert_eq!(*rec.0.lock().unwrap(), vec!["checkpoint".to_string()]);
    }

    #[test]
    fn does_not_stamp_for_non_gated_tool() {
        let (rec, git) = build_session_git();
        let mut ev = tool_result_event("call-2");
        enrich_checkpoint(
            &mut ev,
            |cid| (cid == "call-2").then(|| "read".to_string()),
            &git,
            Path::new("/tmp/cb"),
            1,
        );
        let Some(event::Body::Payload(p)) = ev.body else {
            panic!("expected payload body");
        };
        let Some(payload::Kind::ToolResult(tr)) = p.kind else {
            panic!("expected ToolResult kind");
        };
        assert!(
            tr.checkpoint.is_none(),
            "non-gated tool must not be stamped"
        );
        // No checkpoint call was made — empty SHA fallback path verified too.
        assert!(rec.0.lock().unwrap().is_empty());
    }

    #[test]
    fn stamps_checkpoint_inside_replace_wrapped_toolresult() {
        let (rec, git) = build_session_git();
        let mut ev = Event {
            body: Some(event::Body::Replace(Replace {
                target: "orig-id".to_string(),
                replacement: Some(Payload {
                    kind: Some(payload::Kind::ToolResult(ToolResult {
                        call_id: "call-3".to_string(),
                        ..Default::default()
                    })),
                }),
                reason: None,
            })),
            ..Default::default()
        };
        enrich_checkpoint(
            &mut ev,
            |cid| (cid == "call-3").then(|| "shell".to_string()),
            &git,
            Path::new("/tmp/cb"),
            1,
        );
        let Some(event::Body::Replace(replace)) = ev.body else {
            panic!("expected Replace body");
        };
        let replacement = replace.replacement.expect("replacement must be present");
        let Some(payload::Kind::ToolResult(tr)) = replacement.kind else {
            panic!("expected ToolResult inside replacement");
        };
        assert_eq!(tr.checkpoint.as_deref(), Some("deadbeef"));
        assert_eq!(*rec.0.lock().unwrap(), vec!["checkpoint".to_string()]);
    }
}
