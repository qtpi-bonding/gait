//! `StepOutcome` + table-driven `finalize()` — Stage 3 / Task 6.
//!
//! `finalize()` is THE epilogue: every one of `run_turn`'s 13 return sites
//! (Tasks 7–13) will collapse into `return finalize(&env, &mut state, &persist,
//! outcome).await` for the matching `StepOutcome`. This task only defines the
//! type and proves the table against unit-mocked hooks/repos — nothing in
//! `run_turn` calls `finalize()` yet.
//!
//! ## The table (verbatim from plan-01 §Stage 3 Task 6)
//!
//! | outcome | terminal event | checkpoint | commit | post_turn | drain |
//! |---|---|---|---|---|---|
//! | `Done` | `turn_complete` | ✓ | ✓ | "done" | — |
//! | `Pause(Permissions)` | `permissions` | — | ✓ | "permissions" | — |
//! | `Pause(Clarify)` | — | — | ✓ | "clarify" | — |
//! | `Pause(Inform)` | — | — | ✓ | "inform" | — |
//! | `Pause(SandboxUnlock)` | `sandbox_unlock` | — | ✓ | "unlock" | — |
//! | `Cancelled` | `cancellation_signal` | — | ✓ | *(skipped today — inconsistency, preserved)* | — |
//! | `MaxSteps` | — | — | *(drain-commit only, swallowed)* | — | ✓ |
//! | `Fail(err)` | *(none)* | — | *(none)* | *(none)* | — |
//!
//! `MaxSteps` is a dedicated variant, not folded into `Fail` (plan review M2):
//! the drain is a distinct 3-part sequence (record pending completions →
//! `commit_swallow` → `spawn_wake`), not a `persist.commit`.
//!
//! **`Done` ordering (plan review M1):** checkpoint → diff → build `changes` →
//! emit `turn_complete` (carrying `checkpoint`/`changes`) → `persist.commit` →
//! `post_turn`. Getting this order wrong silently drops `checkpoint`/`changes`
//! from the transcript — the table's column order is a policy summary, not
//! the execution order.
//!
//! **`step_count` provenance (plan review m4):** `Done` uses `step_index + 1`
//! in `run_turn` today; `Permissions`/`Clarify`/`Inform` use
//! `final_step_index + 1`. These are equal only because `final_step_index =
//! step_index` is assigned at the top of the loop body — `finalize` folds
//! both to `state.final_step_index + 1`. This coupling must be preserved
//! until Task 19 (see that task's note on why `final_step_index` stays in
//! the driver loop).

use crate::git::{build_trace_message, TraceTrailers};
use crate::proto::events;
use crate::proto::gait::log::v1 as pb;
use crate::proto::gait::log::v1::TurnTokenUsage;
use crate::session::log::count_turn_complete;
use crate::text::split_into_blocks;
use crate::types::HookContext;

use super::persist::Persistence;
use super::state::{TurnEnv, TurnState};
use super::TurnResult;

/// The four ways a step can pause the turn for external input.
#[allow(dead_code)] // Threaded through starting Task 7; no producer yet.
pub(crate) enum PauseKind {
    Permissions,
    Clarify(Vec<pb::ClarifyItem>),
    Inform(Vec<pb::InformItem>),
    SandboxUnlock(Vec<pb::SandboxUnlockItem>),
}

/// What a step produced, in a form `finalize` can dispatch on.
///
/// `Continue` is never passed to `finalize` — the driver loop consumes it
/// directly (see Task 13, which makes the loop itself outcome-driven).
#[allow(dead_code)] // Threaded through starting Task 7; no producer yet.
pub(crate) enum StepOutcome {
    Continue,
    /// Implicit (plain-text) or structured (`structured_done`) completion.
    /// `changes`/`turn_n` are deliberately NOT carried here — `finalize`
    /// recomputes them via `persist.checkpoint` so the checkpoint-then-emit
    /// ordering (M1) lives in exactly one place.
    Done {
        message: String,
        result: Option<serde_json::Value>,
        /// `true` = implicit completion (model emitted no tool calls);
        /// `false` = explicit `structured_done`/`output()`.
        implicit: bool,
    },
    Pause(PauseKind),
    Cancelled,
    /// Stream error, schema exhaustion, subagent-no-done. No epilogue at all
    /// today — preserved verbatim, not "fixed" (that's Stage 6, out of scope).
    Fail(anyhow::Error),
    /// Natural loop exhaustion. Runs the final-drain (record pending
    /// completions → swallowed commit → wake) then fails the turn.
    /// Carries the resolved `max_iter` for the exact bail message.
    MaxSteps {
        max_iter: usize,
    },
}

/// THE epilogue. Every `run_turn` return site funnels through here once
/// Tasks 7–13 land (this task only proves the table works in isolation).
#[allow(dead_code)] // Called from run_turn starting Task 7.
pub(crate) async fn finalize(
    env: &TurnEnv<'_>,
    state: &mut TurnState,
    persist: &Persistence<'_>,
    outcome: StepOutcome,
) -> anyhow::Result<TurnResult> {
    match outcome {
        StepOutcome::Continue => {
            unreachable!("finalize is never called with StepOutcome::Continue")
        }

        StepOutcome::Done {
            message,
            result,
            implicit,
        } => {
            // M1: checkpoint → diff → changes → emit turn_complete → commit → post_turn.
            let events_now = env.ingestor.events()?;
            let turn_n = count_turn_complete(&events_now) as u32 + 1;
            let code_checkpoint = persist.checkpoint(turn_n);
            let changes = code_checkpoint.as_ref().and_then(|sha| {
                env.repos
                    .checkpoints
                    .diff(env.gait_dir, env.session_id, sha)
                    .ok()
                    .flatten()
            });
            let usage = TurnTokenUsage {
                input: state.total_usage.input_tokens as u64,
                output: state.total_usage.output_tokens as u64,
                cache_read: state.total_usage.cache_read_tokens as u64,
                cache_write: state.total_usage.cache_write_tokens as u64,
            };
            env.emitter
                .record(events::turn_complete(events::TurnCompleteParams {
                    usage,
                    codebase_hash: env.codebase_hash.to_string(),
                    codebase_dirty: env.codebase_dirty,
                    checkpoint: code_checkpoint,
                    blocks: split_into_blocks(&state.all_turn_text),
                    message: message.clone(),
                    result: result.clone(),
                    changes: changes.clone(),
                }))?;
            persist.commit(&build_trace_message(
                &format!("session({}): turn {}", env.session_id, turn_n),
                &TraceTrailers {
                    phase: "turn".to_string(),
                    turn: Some(turn_n),
                    parent_session: None,
                    subagent_session: None,
                    status: None,
                    codebase: env.codebase_path.display().to_string(),
                    codebase_hash: env.codebase_hash.to_string(),
                },
            ))?;
            post_turn(
                env,
                "done",
                state.final_step_index + 1,
                Some(message.clone()),
            );
            Ok(TurnResult::Done {
                message,
                result,
                changes,
                implicit,
            })
        }

        StepOutcome::Pause(PauseKind::Permissions) => {
            // Caller must have already recorded `pending_permissions` on `state`
            // via `events::permissions(..)` before calling finalize — the event
            // itself carries the actual item list, which StepOutcome does not.
            persist.commit(&build_trace_message(
                &format!("session({}): permissions", env.session_id),
                &TraceTrailers {
                    phase: "permissions".to_string(),
                    codebase: env.codebase_path.display().to_string(),
                    codebase_hash: env.codebase_hash.to_string(),
                    ..Default::default()
                },
            ))?;
            post_turn(env, "permissions", state.final_step_index + 1, None);
            Ok(TurnResult::Permissions)
        }

        StepOutcome::Pause(PauseKind::Clarify(items)) => {
            // No terminal event here — the `clarify` event was already recorded
            // by the signal-handling code that produced this outcome (today:
            // the `ExecutionSignal::Clarify` arm). Preserved verbatim.
            persist.commit(&build_trace_message(
                &format!("session({}): clarify", env.session_id),
                &TraceTrailers {
                    phase: "clarify".to_string(),
                    codebase: env.codebase_path.display().to_string(),
                    codebase_hash: env.codebase_hash.to_string(),
                    ..Default::default()
                },
            ))?;
            post_turn(env, "clarify", state.final_step_index + 1, None);
            Ok(TurnResult::Clarify { items })
        }

        StepOutcome::Pause(PauseKind::Inform(items)) => {
            persist.commit(&build_trace_message(
                &format!("session({}): inform", env.session_id),
                &TraceTrailers {
                    phase: "inform".to_string(),
                    codebase: env.codebase_path.display().to_string(),
                    codebase_hash: env.codebase_hash.to_string(),
                    ..Default::default()
                },
            ))?;
            post_turn(env, "inform", state.final_step_index + 1, None);
            Ok(TurnResult::Inform { items })
        }

        StepOutcome::Pause(PauseKind::SandboxUnlock(items)) => {
            env.emitter.record(events::sandbox_unlock(&items))?;
            persist.commit(&build_trace_message(
                &format!("session({}): unlock", env.session_id),
                &TraceTrailers {
                    phase: "unlock".to_string(),
                    codebase: env.codebase_path.display().to_string(),
                    codebase_hash: env.codebase_hash.to_string(),
                    ..Default::default()
                },
            ))?;
            post_turn(env, "unlock", state.final_step_index + 1, None);
            Ok(TurnResult::SandboxUnlock { items })
        }

        StepOutcome::Cancelled => {
            env.emitter.record(events::cancellation_signal())?;
            persist.commit(&build_trace_message(
                &format!("session({}): cancelled", env.session_id),
                &TraceTrailers {
                    phase: "cancelled".to_string(),
                    codebase: env.codebase_path.display().to_string(),
                    codebase_hash: env.codebase_hash.to_string(),
                    ..Default::default()
                },
            ))?;
            // Inconsistency preserved verbatim (per plan Global Constraints):
            // cancellation commits but skips post_turn. Normalizing this is
            // Stage 6, explicitly out of scope here.
            Ok(TurnResult::Cancelled)
        }

        StepOutcome::Fail(err) => {
            // No event, no commit, no post_turn — preserved.
            // Now a typed outcome instead of a bare Err: the turn still counts
            // as failed (exit 1), but `ended` carries the reason on stdout.
            Ok(TurnResult::Failed {
                ended: "error".into(),
                message: err.to_string(),
            })
        }

        StepOutcome::MaxSteps { max_iter } => {
            if persist.is_unsealed() {
                let events_snap = env.ingestor.events().unwrap_or_default();
                let already_d =
                    crate::turn::completion_poll::surfaced_daemon_idempotency_keys(&events_snap);
                let pending_d = crate::turn::completion_poll::pending_daemon_completions(
                    env.gait_dir,
                    env.session_id,
                    &already_d,
                );
                let already_a =
                    crate::turn::completion_poll::surfaced_agent_idempotency_keys(&events_snap);
                let pending_a = crate::turn::completion_poll::pending_agent_completions(
                    env.gait_dir,
                    env.session_id,
                    &already_a,
                );
                let has_pending = !pending_d.is_empty() || !pending_a.is_empty();
                if has_pending {
                    for c in pending_d.iter().chain(pending_a.iter()) {
                        let _ = env.emitter.record(c.body());
                    }
                    persist.commit_swallow(&format!(
                        "session({}): final-drain completions",
                        env.session_id
                    ));
                    crate::launch::spawn_wake(&crate::session::completion::WakeRequest {
                        gait_dir: std::path::PathBuf::from(env.gait_dir),
                        parent_session: env.session_id.to_string(),
                    });
                }
            }
            Ok(TurnResult::Failed {
                ended: "max_steps".into(),
                message: format!("reached max steps ({max_iter}) without turn_complete"),
            })
        }
    }
}

/// Fire every hook's `post_turn`, building the shared `HookContext` fields
/// once. `final_response` is only set on the `Done` path (matches today's
/// `..Default::default()` epilogues, which leave it empty otherwise).
fn post_turn(
    env: &TurnEnv<'_>,
    turn_outcome: &str,
    step_count: usize,
    final_response: Option<String>,
) {
    let ctx = HookContext {
        event: "post_turn".to_string(),
        session_id: env.session_id.to_string(),
        gait_dir: env.gait_dir.to_path_buf(),
        codebase: env.codebase_path.to_path_buf(),
        system_prompt: env.system_prompt.to_string(),
        step_count,
        turn_outcome: turn_outcome.to_string(),
        final_response: final_response.unwrap_or_default(),
        ..Default::default()
    };
    for h in env.hooks {
        h.post_turn(&ctx, env.emitter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::checkpoint::MockCheckpointRepo;
    use crate::git::trace::TraceRepo;
    use crate::git::Repos;
    use crate::session::Ingestor;
    use crate::session::Session;
    use crate::types::Hook;
    use gait_provider::provider::{ChunkStream, Provider, ProviderError, StreamChunk};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    struct NoopProvider;
    #[async_trait::async_trait]
    impl Provider for NoopProvider {
        fn model_id(&self) -> &str {
            "noop"
        }
        async fn stream(
            &self,
            _system: &str,
            _messages: &[crate::types::Message],
            _tools: &[Box<dyn crate::types::Tool>],
            _params: &crate::config::LlmParamsConfig,
            _tail_context: Option<&str>,
        ) -> Result<ChunkStream, ProviderError> {
            let _ = StreamChunk::Done;
            Err(ProviderError::Other("unused in finalize tests".into()))
        }
    }

    /// Spy `TraceRepo` recording every `commit_session` message.
    struct SpyTrace {
        commits: Mutex<Vec<String>>,
    }
    impl SpyTrace {
        fn new() -> Self {
            Self {
                commits: Mutex::new(Vec::new()),
            }
        }
        fn commits(&self) -> Vec<String> {
            self.commits.lock().unwrap().clone()
        }
    }
    impl TraceRepo for SpyTrace {
        fn init_workspace(&self, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_session_worktree(&self, _: &Path, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn commit_session(&self, _: &Path, _: &str, message: &str) -> anyhow::Result<()> {
            self.commits.lock().unwrap().push(message.to_string());
            Ok(())
        }
        fn list_active_sessions(&self, _: &Path) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn finish_session(&self, _: &Path, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_branch_worktree(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: &Path,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove_branch_worktree(&self, _: &Path, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn merge_branch_ff(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn discard_session(&self, _gait_dir: &Path, _session_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Spy `Hook` recording every `post_turn` outcome string.
    struct SpyHook {
        outcomes: Mutex<Vec<String>>,
    }
    impl SpyHook {
        fn new() -> Self {
            Self {
                outcomes: Mutex::new(Vec::new()),
            }
        }
        fn outcomes(&self) -> Vec<String> {
            self.outcomes.lock().unwrap().clone()
        }
    }
    #[async_trait::async_trait]
    impl Hook for SpyHook {
        fn post_turn(&self, ctx: &HookContext, _rec: &dyn crate::types::Rec) {
            self.outcomes.lock().unwrap().push(ctx.turn_outcome.clone());
        }
    }

    /// Everything a `finalize()` test needs, alive for the whole test body.
    struct Fixture {
        _ws: tempfile::TempDir,
        session: Session,
        emitter: crate::Recorder,
        ingestor: Ingestor,
        provider: Arc<dyn Provider>,
        hooks: Vec<Box<dyn Hook>>,
        sandbox_cfg: crate::config::SandboxConfig,
        deny: Vec<String>,
        daemons: Vec<(String, i32, bool, String)>,
        system_prompt: String,
        codebase_hash: String,
        llm_params: crate::config::LlmParamsConfig,
        effective_allow: Vec<String>,
        model: String,
        mcp_registry: Vec<crate::config::McpToolDef>,
        mcp_clients: std::collections::HashMap<String, Arc<crate::mcp::McpClient>>,
        plugin_registry: Vec<crate::config::WasmPluginDef>,
        result_schema: Option<serde_json::Value>,
        spy_hook: Arc<SpyHook>,
        trace: Arc<SpyTrace>,
    }

    fn fixture() -> Fixture {
        let ws = tempfile::TempDir::new().unwrap();
        let session_id = "sfin01".to_string();
        let trace = Arc::new(SpyTrace::new());
        let repos = Repos {
            codebase: Arc::new(crate::git::codebase::GitCodebaseRepo),
            traces: trace.clone(),
            checkpoints: Arc::new(MockCheckpointRepo::new()),
        };
        let session = Session {
            id: session_id.clone(),
            gait_dir: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            assigned_by: None,
            repos,
        };
        let spy_hook = Arc::new(SpyHook::new());
        let hooks: Vec<Box<dyn Hook>> = vec![Box::new(SpyHookHandle(spy_hook.clone()))];
        Fixture {
            _ws: ws,
            session,
            emitter: crate::Recorder::buffering(),
            ingestor: Ingestor::from_shared(Arc::new(Mutex::new(Vec::new()))),
            provider: Arc::new(NoopProvider),
            hooks,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            deny: vec![],
            daemons: vec![],
            system_prompt: "sys".to_string(),
            codebase_hash: "deadbeef".to_string(),
            llm_params: crate::config::LlmParamsConfig::default(),
            effective_allow: vec![],
            model: "test/model".to_string(),
            mcp_registry: vec![],
            mcp_clients: std::collections::HashMap::new(),
            plugin_registry: vec![],
            result_schema: None,
            spy_hook,
            trace,
        }
    }

    /// `Hook` isn't `Clone`, so the fixture holds an `Arc<SpyHook>` for
    /// assertions and hands `finalize` a thin forwarding handle.
    struct SpyHookHandle(Arc<SpyHook>);
    #[async_trait::async_trait]
    impl Hook for SpyHookHandle {
        fn post_turn(&self, ctx: &HookContext, rec: &dyn crate::types::Rec) {
            self.0.post_turn(ctx, rec);
        }
    }

    fn env_from<'a>(fx: &'a Fixture) -> TurnEnv<'a> {
        TurnEnv {
            session: &fx.session,
            emitter: &fx.emitter,
            repos: &fx.session.repos,
            gait_dir: &fx.session.gait_dir,
            codebase_path: &fx.session.codebase_path,
            session_id: &fx.session.id,
            provider: &fx.provider,
            hooks: &fx.hooks,
            sandbox_cfg: &fx.sandbox_cfg,
            sandbox_on: false,
            cancel: None,
            deny: &fx.deny,
            preview_policy: crate::config::PreviewPolicy::default(),
            auto: true,
            daemons: &fx.daemons,
            ingestor: &fx.ingestor,
            system_prompt: &fx.system_prompt,
            codebase_hash: &fx.codebase_hash,
            codebase_dirty: false,
            llm_params: &fx.llm_params,
            effective_allow: &fx.effective_allow,
            relay: None,
            model: &fx.model,
            mcp_registry: &fx.mcp_registry,
            mcp_clients: &fx.mcp_clients,
            plugin_registry: &fx.plugin_registry,
            result_schema: &fx.result_schema,
        }
    }

    fn unsealed<'a>(fx: &'a Fixture) -> Persistence<'a> {
        Persistence::Unsealed {
            repos: &fx.session.repos,
            gait_dir: &fx.session.gait_dir,
            session_id: &fx.session.id,
            codebase_path: &fx.session.codebase_path,
        }
    }

    #[tokio::test]
    async fn finalize_done_checkpoints_commits_emits_turn_complete_and_posts_done() {
        let fx = fixture();
        let env = env_from(&fx);
        let persist = unsealed(&fx);
        let mut state = TurnState::new(&crate::profile::ResolvedProfile::for_test(
            fx.provider.clone(),
            vec![],
            vec![],
            vec![],
        ));

        let result = finalize(
            &env,
            &mut state,
            &persist,
            StepOutcome::Done {
                message: "all done".into(),
                result: None,
                implicit: true,
            },
        )
        .await
        .expect("Done must finalize successfully");

        assert!(matches!(result, TurnResult::Done { message, .. } if message == "all done"));
        assert_eq!(fx.spy_hook.outcomes(), vec!["done".to_string()]);
        assert_eq!(
            fx.trace.commits(),
            vec![format!(
                "session(sfin01): turn 1\n\nPhase: turn\nTurn: 1\nCodebase: {}\nCodebase-Hash: deadbeef",
                fx.session.codebase_path.display()
            )]
        );

        let events = fx.emitter.take_buffer();
        assert_eq!(events.len(), 1, "exactly one turn_complete event");
        let is_turn_complete = matches!(
            events[0].body.as_ref(),
            Some(crate::proto::gait::log::v1::event::Body::Payload(p))
                if matches!(p.kind, Some(crate::proto::gait::log::v1::payload::Kind::TurnComplete(_)))
        );
        assert!(is_turn_complete, "expected a TurnComplete body");
    }

    #[tokio::test]
    async fn finalize_permissions_commits_no_terminal_event_and_posts_permissions() {
        let fx = fixture();
        let env = env_from(&fx);
        let persist = unsealed(&fx);
        let mut state = TurnState::new(&crate::profile::ResolvedProfile::for_test(
            fx.provider.clone(),
            vec![],
            vec![],
            vec![],
        ));

        let result = finalize(
            &env,
            &mut state,
            &persist,
            StepOutcome::Pause(PauseKind::Permissions),
        )
        .await
        .unwrap();
        assert!(matches!(result, TurnResult::Permissions));
        assert_eq!(fx.spy_hook.outcomes(), vec!["permissions".to_string()]);
        assert_eq!(
            fx.trace.commits(),
            vec![format!(
                "session(sfin01): permissions\n\nPhase: permissions\nCodebase: {}\nCodebase-Hash: deadbeef",
                fx.session.codebase_path.display()
            )]
        );
        assert!(
            fx.emitter.take_buffer().is_empty(),
            "finalize itself emits no event for Permissions — the caller already did"
        );
    }

    #[tokio::test]
    async fn finalize_clarify_commits_and_posts_clarify_no_terminal_event() {
        let fx = fixture();
        let env = env_from(&fx);
        let persist = unsealed(&fx);
        let mut state = TurnState::new(&crate::profile::ResolvedProfile::for_test(
            fx.provider.clone(),
            vec![],
            vec![],
            vec![],
        ));
        let items = vec![pb::ClarifyItem {
            id: "i1".into(),
            content: "q?".into(),
            why: "why".into(),
            kind: pb::ClarifyKind::Is as i32,
            options: vec![],
        }];

        let result = finalize(
            &env,
            &mut state,
            &persist,
            StepOutcome::Pause(PauseKind::Clarify(items.clone())),
        )
        .await
        .unwrap();
        assert!(matches!(result, TurnResult::Clarify { .. }));
        assert_eq!(fx.spy_hook.outcomes(), vec!["clarify".to_string()]);
        assert_eq!(
            fx.trace.commits(),
            vec![format!(
                "session(sfin01): clarify\n\nPhase: clarify\nCodebase: {}\nCodebase-Hash: deadbeef",
                fx.session.codebase_path.display()
            )]
        );
        assert!(fx.emitter.take_buffer().is_empty());
    }

    #[tokio::test]
    async fn finalize_inform_commits_and_posts_inform_no_terminal_event() {
        let fx = fixture();
        let env = env_from(&fx);
        let persist = unsealed(&fx);
        let mut state = TurnState::new(&crate::profile::ResolvedProfile::for_test(
            fx.provider.clone(),
            vec![],
            vec![],
            vec![],
        ));
        let items = vec![pb::InformItem {
            id: "i1".into(),
            content: "fyi".into(),
            context: None,
        }];

        let result = finalize(
            &env,
            &mut state,
            &persist,
            StepOutcome::Pause(PauseKind::Inform(items.clone())),
        )
        .await
        .unwrap();
        assert!(matches!(result, TurnResult::Inform { .. }));
        assert_eq!(fx.spy_hook.outcomes(), vec!["inform".to_string()]);
        assert_eq!(
            fx.trace.commits(),
            vec![format!(
                "session(sfin01): inform\n\nPhase: inform\nCodebase: {}\nCodebase-Hash: deadbeef",
                fx.session.codebase_path.display()
            )]
        );
        assert!(fx.emitter.take_buffer().is_empty());
    }

    #[tokio::test]
    async fn finalize_unlock_emits_event_commits_and_posts_unlock() {
        let fx = fixture();
        let env = env_from(&fx);
        let persist = unsealed(&fx);
        let mut state = TurnState::new(&crate::profile::ResolvedProfile::for_test(
            fx.provider.clone(),
            vec![],
            vec![],
            vec![],
        ));
        let items = vec![pb::SandboxUnlockItem {
            id: "u1".into(),
            tool_name: "shell".into(),
            resource: "/x".into(),
            dimension: "write".into(),
            preview: None,
        }];

        let result = finalize(
            &env,
            &mut state,
            &persist,
            StepOutcome::Pause(PauseKind::SandboxUnlock(items.clone())),
        )
        .await
        .unwrap();
        assert!(matches!(result, TurnResult::SandboxUnlock { .. }));
        assert_eq!(fx.spy_hook.outcomes(), vec!["unlock".to_string()]);
        assert_eq!(
            fx.trace.commits(),
            vec![format!(
                "session(sfin01): unlock\n\nPhase: unlock\nCodebase: {}\nCodebase-Hash: deadbeef",
                fx.session.codebase_path.display()
            )]
        );
        assert_eq!(fx.emitter.take_buffer().len(), 1, "unlock event recorded");
    }

    #[tokio::test]
    async fn finalize_cancelled_emits_event_commits_but_skips_post_turn() {
        let fx = fixture();
        let env = env_from(&fx);
        let persist = unsealed(&fx);
        let mut state = TurnState::new(&crate::profile::ResolvedProfile::for_test(
            fx.provider.clone(),
            vec![],
            vec![],
            vec![],
        ));

        let result = finalize(&env, &mut state, &persist, StepOutcome::Cancelled)
            .await
            .unwrap();
        assert!(matches!(result, TurnResult::Cancelled));
        assert!(
            fx.spy_hook.outcomes().is_empty(),
            "Cancelled skips post_turn today — preserved inconsistency"
        );
        assert_eq!(
            fx.emitter.take_buffer().len(),
            1,
            "cancellation_signal event recorded"
        );
        assert_eq!(
            fx.trace.commits(),
            vec![format!(
                "session(sfin01): cancelled\n\nPhase: cancelled\nCodebase: {}\nCodebase-Hash: deadbeef",
                fx.session.codebase_path.display()
            )]
        );
    }

    #[tokio::test]
    async fn finalize_fail_returns_typed_failed() {
        let fx = fixture();
        let env = env_from(&fx);
        let persist = unsealed(&fx);
        let mut state = TurnState::new(&crate::profile::ResolvedProfile::for_test(
            fx.provider.clone(),
            vec![],
            vec![],
            vec![],
        ));

        let result = finalize(
            &env,
            &mut state,
            &persist,
            StepOutcome::Fail(anyhow::anyhow!("stream error: boom")),
        )
        .await
        .expect("Fail now returns Ok(Failed), not Err");
        assert!(
            matches!(&result, TurnResult::Failed { ended, message }
                if ended == "error" && message.contains("boom")),
            "expected Ok(Failed{{error, message contains boom}}), got: {result:?}"
        );
        assert!(fx.spy_hook.outcomes().is_empty());
        assert!(fx.emitter.take_buffer().is_empty());
    }

    #[tokio::test]
    async fn finalize_max_steps_returns_typed_failed() {
        let fx = fixture();
        let env = env_from(&fx);
        let persist = unsealed(&fx);
        let mut state = TurnState::new(&crate::profile::ResolvedProfile::for_test(
            fx.provider.clone(),
            vec![],
            vec![],
            vec![],
        ));

        let result = finalize(
            &env,
            &mut state,
            &persist,
            StepOutcome::MaxSteps { max_iter: 100 },
        )
        .await
        .expect("MaxSteps now returns Ok(Failed), not Err");
        assert!(
            matches!(&result, TurnResult::Failed { ended, message }
                if ended == "max_steps" && message.contains("max steps (100)")),
            "expected Ok(Failed{{max_steps, max steps (100)}}), got: {result:?}"
        );
        assert!(fx.spy_hook.outcomes().is_empty());
        // No pending daemon/agent completions in this fixture (empty ingestor,
        // no daemons.jsonl) — the drain body is a no-op, so no commit either.
    }
}
