//! The real turn executor for script runs.
//!
//! `run_script` takes an `Arc<dyn TurnExecutor>`. [`ScriptTurnExecutor`] is
//! the real one: it fills the crate-internal [`crate::turn_exec::TurnSpec`]
//! from a script request and hands it to `turn_run_opts` / `drive_turn` —
//! exactly one place builds `TurnRunOpts`, exactly one place runs a turn and
//! maps its outcome.

use std::path::PathBuf;
use std::sync::Mutex;

use tokio::sync::mpsc::UnboundedSender;

use crate::turn_exec::{
    drive_turn, turn_run_opts, TurnExecutor, TurnOutcome, TurnRequest, TurnSpec,
};

/// The real executor for script runs: drives one sealed turn through
/// `gait-turn-runner` off a [`TurnRequest`], via the shared spec/opts/driver
/// path.
pub struct ScriptTurnExecutor {
    gait_dir: PathBuf,
    codebase: PathBuf,
    run_id: String,
    /// The output channel the run driver attached (see
    /// `TurnExecutor::attach_output`). Held in a `Mutex` so the executor
    /// stays `Send + Sync` under `#[async_trait(?Send)]` — the attach call
    /// arrives from the run driver's thread, the turn runs on the script's.
    output: Mutex<Option<UnboundedSender<gait_core::embed::SinkFrame>>>,
}

impl ScriptTurnExecutor {
    pub fn new(gait_dir: PathBuf, codebase: PathBuf, run_id: String) -> Self {
        Self {
            gait_dir,
            codebase,
            run_id,
            output: Mutex::new(None),
        }
    }

    /// Build `TurnRunOpts` for a request WITHOUT running a turn — the single
    /// place this executor maps request fields onto a `TurnSpec`
    /// (`run_turn` uses it too, then drives through `drive_turn`).
    pub(crate) fn opts_for(
        &self,
        req: &TurnRequest,
    ) -> anyhow::Result<gait_turn_runner::TurnRunOpts> {
        // The host chooses the step's session id once (`req.session_id`,
        // `continueSession` or a fresh id) so retries reuse it; when the
        // request carries none (a direct executor call), fall back to the
        // continue marker or a fresh id here.
        let session_id = if !req.session_id.is_empty() {
            req.session_id.clone()
        } else if let Some(cs) = &req.continue_session {
            if !cs.is_empty() {
                cs.clone()
            } else {
                gait_core::session::new_session_id()
            }
        } else {
            gait_core::session::new_session_id()
        };
        let continue_session = match &req.continue_session {
            Some(cs) if !cs.is_empty() => cs.clone(),
            _ => String::new(),
        };
        // A script's `model` names a gait inference: resolution is checked, so
        // an unknown name fails with the resolver's error rather than being
        // silently ignored.
        let codebase = match &req.codebase {
            Some(cb) if cb.is_absolute() => cb.clone(),
            Some(cb) => self.codebase.join(cb),
            None => self.codebase.clone(),
        };
        turn_run_opts(TurnSpec {
            per_call_inference: req.model.clone(),
            per_call_allow: if req.allow.is_empty() {
                None
            } else {
                Some(req.allow.clone())
            },
            per_call_agent: req.agent.clone(),
            auto: req.auto.unwrap_or(false),
            result_schema: req.output_schema.clone(),
            codebase,
            session_id,
            continue_session,
            run_id: self.run_id.clone(),
            gait_dir: self.gait_dir.clone(),
        })
    }
}

#[async_trait::async_trait(?Send)]
impl TurnExecutor for ScriptTurnExecutor {
    async fn run_turn(&self, req: TurnRequest) -> anyhow::Result<TurnOutcome> {
        // A request without a prompt runs nothing and leaves the node open.
        let Some(prompt) = req.prompt.clone() else {
            return Ok(TurnOutcome {
                open: true,
                ..TurnOutcome::default()
            });
        };
        // No output channel attached is a wiring bug in the run driver, not a
        // condition to paper over: surface it instead of silently dropping
        // the turn's sink output.
        let frames = match self
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            Some(tx) => tx.clone(),
            None => anyhow::bail!("script turn executor has no output channel attached"),
        };
        let opts = self.opts_for(&req)?;
        drive_turn(opts, prompt, frames).await
    }

    fn attach_output(&self, frames: UnboundedSender<gait_core::embed::SinkFrame>) {
        *self
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A temp gait dir with two authored inferences ("mock" and "other") and
    /// "mock" as the configured default. Everything lives under the temp dir;
    /// the real HOME is never read (`load_config` only touches `gait_dir`,
    /// and `opts_for` never touches an inference's contents).
    fn setup_gait_dir() -> tempfile::TempDir {
        let ws = tempfile::tempdir().unwrap();
        let gait_dir = ws.path().to_path_buf();
        for name in ["mock", "other"] {
            std::fs::create_dir_all(gait_dir.join("inferences").join(name)).unwrap();
            std::fs::write(
                gait_dir
                    .join("inferences")
                    .join(name)
                    .join("inference.textproto"),
                "model: \"mock/model\"\ncompaction_threshold_tokens: 128000\n",
            )
            .unwrap();
        }
        std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "mock" }"#).unwrap();
        ws
    }

    fn executor(gait_dir: &std::path::Path) -> ScriptTurnExecutor {
        ScriptTurnExecutor::new(
            gait_dir.to_path_buf(),
            gait_dir.join("codebase"),
            "run-1".to_string(),
        )
    }

    fn base_req() -> TurnRequest {
        TurnRequest {
            id: "t1".to_string(),
            prompt: Some("do the thing".to_string()),
            ..TurnRequest::default()
        }
    }

    #[test]
    fn script_turn_model_names_the_inference() {
        let ws = setup_gait_dir();
        let ex = executor(ws.path());

        // A named model overrides the configured default inference.
        let mut req = base_req();
        req.model = Some("other".to_string());
        let opts = ex.opts_for(&req).unwrap();
        let profile = opts.profile.as_ref().unwrap();
        assert_eq!(profile.inference.as_deref(), Some("other"));

        // No model → the configured default from gait.jsonc.
        let req = base_req();
        let opts = ex.opts_for(&req).unwrap();
        let profile = opts.profile.as_ref().unwrap();
        assert_eq!(profile.inference.as_deref(), Some("mock"));
    }

    #[test]
    fn script_turn_maps_request_fields() {
        let ws = setup_gait_dir();
        let ex = executor(ws.path());
        let mut req = base_req();
        req.allow = vec!["read".to_string(), "write".to_string()];
        req.agent = Some("reviewer".to_string());
        req.auto = Some(true);
        req.output_schema = Some(json!({"type": "object"}));
        req.codebase = Some(PathBuf::from("sub/worktree"));
        let opts = ex.opts_for(&req).unwrap();
        let profile = opts.profile.as_ref().unwrap();
        assert_eq!(profile.allow, vec!["read".to_string(), "write".to_string()]);
        assert_eq!(profile.agent.as_deref(), Some("reviewer"));
        assert_eq!(profile.auto, Some(true));
        assert_eq!(
            profile.result_schema.as_ref(),
            Some(&json!({"type": "object"}))
        );
        // A relative codebase path is joined onto the executor's codebase.
        assert_eq!(
            opts.codebase,
            ws.path().join("codebase").join("sub/worktree")
        );
        // run_id is the executor's run id; sandbox on; no cascade/delegate/
        // intree.
        assert_eq!(opts.run_id, "run-1");
        assert_eq!(opts.sandbox_on, Some(true));
        assert!(opts.cascade.is_none());
        assert!(opts.cascade_group_arg.is_none());
        assert!(opts.fs_delegate.is_none());
        assert!(!opts.allow_intree);
    }

    #[test]
    fn script_turn_session_is_continued_or_fresh() {
        let ws = setup_gait_dir();
        let ex = executor(ws.path());

        // With a continue_session, the continue marker is it.
        let mut req = base_req();
        req.continue_session = Some("prior".to_string());
        let opts = ex.opts_for(&req).unwrap();
        assert_eq!(opts.continue_session, "prior");
        assert_eq!(opts.session_id, "prior");

        // The request's session id (chosen once by the host) is the session.
        let mut req = base_req();
        req.session_id = "host-chosen".to_string();
        let opts = ex.opts_for(&req).unwrap();
        assert_eq!(opts.session_id, "host-chosen");
        assert_eq!(opts.continue_session, "");

        // Without one: a fresh session id, empty continue marker.
        let req = base_req();
        let first = ex.opts_for(&req).unwrap();
        assert!(!first.session_id.is_empty());
        assert_eq!(first.continue_session, "");

        // Two fresh calls give different ids.
        let req = base_req();
        let second = ex.opts_for(&req).unwrap();
        assert_ne!(first.session_id, second.session_id);
    }

    /// A missing output channel is a run-driver wiring bug: `run_turn` must
    /// fail loudly rather than silently dropping the turn's sink output.
    #[tokio::test]
    async fn script_turn_without_output_channel_is_an_error() {
        let ws = setup_gait_dir();
        let ex = executor(ws.path());
        let err = ex.run_turn(base_req()).await.unwrap_err();
        assert!(err.to_string().contains("no output channel"));
    }
}
