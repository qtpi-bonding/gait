//! `TurnState` and `TurnEnv` — the bundled locals of `run_turn()`.
//!
//! These two structs are the State- and Env-halves of the Stage-1 refactor in
//! the turn decomposition design (Tasks 2 and 3).
//! Together they replace the forest of `let mut` locals that `run_turn` used
//! to scatter across its setup section.
//!
//! ## `TurnState` (Task 2)
//!
//! **Mutable loop-carried state.** Built once via `TurnState::new(&caps)` and
//! passed by `&mut` to every phase module. Per plan review M3, this includes
//! `tools` and `entries` (mutated by `LoadContext` mid-loop) — they are state,
//! not read-only env.
//!
//! ## `TurnEnv<'a>` (Task 3)
//!
//! **Read-only borrows** into `run_turn`'s frame. Constructed at the end of
//! setup by `let env = TurnEnv { ... };`. Threaded through to phase modules
//! starting in Stage 4; nothing consumes it yet (Task 3 is purely additive
//! — `#![allow(dead_code)]` on the struct, and the binding is also allowed).
//!
//! Each field corresponds to a local that already exists by the construction
//! point. Fields are added only when a future phase needs them
//! (Reader-1's borrow table).
//!
//! **No behavior changes.** The Stage-0 golden-transcript suite
//! (`src/turn/golden_tests.rs` + `src/turn/snapshots/`) pins every exit kind;
//! a snapshot diff after this refactor indicates a behavior change and must be
//! fixed in the refactor — never in the snapshot.

use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::profile::ResolvedProfile;
use crate::proto::gait::log::v1 as pb;
use crate::types::{LoadableEntry, Message, Tool, Usage};

/// Loop-carried state for one turn of `run_turn`.
///
/// All fields are **mutable** — including the ones the plan §"Stage 1 / Task 2"
/// initially listed as smaller. Per plan review M3, `tools` and `entries` are
/// mutated across iterations (MCP loadable enable/disable at the in-loop
/// `LoadContext` branch, plus the pre-loop loadable projection) and therefore
/// belong in `TurnState`, not in a read-only `TurnEnv`.
///
/// `current_turn` is deliberately **not** a field here — it is a function of
/// the ingestor at the time the loop is entered, and is consumed as a value
/// (passed into `emitter.set_position`) rather than mutated. Keeping it as a
/// local in `run_turn` matches the plan.
pub(crate) struct TurnState {
    /// Number of classifier denials in a row in this turn.
    pub classify_denial_streak: u32,
    /// Shared turn-scoped capability resolver. Nested callers use this path so
    /// capability execution is not coupled to transcript event recording.
    pub capability_dispatcher: Arc<super::capabilities::CapabilityDispatcher>,
    /// Aggregate token usage across all LLM steps of the turn.
    /// Updated via `Usage::add` from each `StreamChunk::StepUsage`.
    pub total_usage: Usage,

    /// Set by an in-loop `ExecutionSignal::Clarify` tool call; consumed at the
    /// bottom of the iteration by the `clarify_signal` exit branch.
    pub clarify_signal: Option<Vec<pb::ClarifyItem>>,

    /// Set by an in-loop `ExecutionSignal::Inform` tool call; consumed at the
    /// bottom of the iteration by the `inform_signal` exit branch.
    pub inform_signal: Option<Vec<pb::InformItem>>,

    /// Concatenation of all step text. Read by the implicit-done and
    /// structured-done completion branches via `split_into_blocks`.
    pub all_turn_text: String,

    /// Index of the most recent step (the loop body's `step_index`). Read by
    /// the Permissions/Clarify/Inform exit branches as `final_step_index + 1`
    /// to populate the `step_count` field of the `post_turn` `HookContext`.
    /// Default 0 covers the zero-iteration case (the loop overwrites it).
    pub final_step_index: usize,

    /// Reconstructed LLM message history. Populated once at startup (resume vs
    /// non-resume paths) and appended to at the end of each step with the
    /// step's assistant message + tool results, so the *next* iteration's
    /// `outgoing` rebuild can fold them in.
    pub messages: Vec<Message>,

    /// Synthetic turn context built once, prepended to every step's
    /// `outgoing` via `prompt::build_session_state`. `None` on the resume
    /// path (the model already has context from the prior turn).
    pub turn_ctx: Option<String>,

    /// Remaining retries for `result_schema` validation failures (the
    /// `schema_retries_remaining` budget on `ExecutionSignal::FinishTurn`).
    /// Seeded from `caps.output_schema_retries`.
    pub schema_retries_remaining: u8,

    /// Remaining retries for the "you must finish by calling
    /// `structured_done`" nudge (the `done_nudges_remaining` budget on
    /// implicit-done). Seeded from `caps.output_schema_retries` — same budget
    /// as `schema_retries_remaining` today.
    pub done_nudges_remaining: u8,

    /// Active tool list. Built in `setup` (caps + projection-driven loadables)
    /// and mutated in-loop by `ExecutionSignal::LoadContext` (MCP server
    /// enable/disable). Read by `provider.stream` each step and by the gate
    /// and scope pre-check.
    pub tools: Vec<Arc<dyn Tool>>,

    /// `<loadable-index>` entries (MCP server / skill / plugin rows) built
    /// once in setup and read every step by
    /// `prompt::build_loadable_enabled_state`.
    pub entries: Vec<LoadableEntry>,
}

/// Read-only borrows into the `run_turn()` frame, used by future phase modules.
///
/// This is the Env-half of the Stage-1 refactor in
/// the turn decomposition design (Task 3). It is a
/// pure mechanical hoist: the read-only borrows that the loop body and the
/// future phase modules (`stream.rs`, `execute.rs`, `signal.rs`,
/// `finalize.rs`) will all need are gathered here so they can be threaded in as a
/// single borrow instead of a forest of &/&mut arguments.
///
/// **No behavior changes.** Each field is a borrow of a local that already exists in
/// `run_turn()` by the construction point. Fields are added only when a future phase
/// actually needs them (per Reader-1's borrow table).
///
/// **Unused for now.** Nothing references `env` yet — that wiring lands in Stage 4
/// (Tasks 14–18). The `#[allow(dead_code)]` suppresses the unused-binding warning.
///
/// All references here are read-only; `TurnState` (above) carries everything mutable.
///
/// Fields are intentionally minimal — only the locals that exist by the time we reach the
/// end of the post-loop setup. `model` and `result_schema` may be added later (Task 17/19)
/// if a phase actually needs them. Same for `mcp_clients`/`skill_registry`/etc. if a phase
/// beyond what's planned ever reads them post-construction.
#[allow(dead_code)] // Threaded through starting Stage 4; no consumer yet.
pub(crate) struct TurnEnv<'a> {
    pub session: &'a crate::session::Session,
    pub emitter: &'a crate::proto::Recorder,
    pub repos: &'a crate::git::Repos,
    pub gait_dir: &'a Path,
    pub codebase_path: &'a Path,
    pub session_id: &'a str,
    pub provider: &'a Arc<dyn gait_provider::provider::Provider>,
    pub hooks: &'a [Box<dyn crate::types::Hook>],
    pub sandbox_cfg: &'a crate::config::SandboxConfig,
    pub sandbox_on: bool,
    pub cancel: Option<&'a CancellationToken>,
    pub deny: &'a [String],
    pub preview_policy: crate::config::PreviewPolicy,
    pub auto: bool,
    pub daemons: &'a [(String, i32, bool, String)],
    pub ingestor: &'a crate::session::Ingestor,
    /// Needed by `finalize()`'s `post_turn` `HookContext` (Task 6).
    pub system_prompt: &'a str,
    /// Git hash supplied to the model-facing status context.
    pub codebase_hash: &'a str,
    pub codebase_dirty: bool,
    /// Needed by `stream::run_step` (Task 16) to build the provider.stream() call.
    pub llm_params: &'a crate::config::LlmParamsConfig,
    pub context_policy: crate::profile::ContextPolicy,
    /// Needed by `execute::run_tools` (Task 17) for gate_classify and
    /// execute_tool_call.
    pub effective_allow: &'a [String],
    pub classify_policy: Option<&'a pb::ClassifyPolicy>,
    pub classifier: &'a dyn super::execute::Classifier,
    pub relay: Option<&'a crate::turn::nerve::NerveWriter>,
    pub model: &'a str,
    pub mcp_registry: &'a [crate::config::McpToolDef],
    pub mcp_clients: &'a crate::profile::McpClients,
    pub plugin_registry: &'a [crate::config::WasmPluginDef],
    pub result_schema: &'a Option<serde_json::Value>,
}

impl TurnState {
    /// Construct a `TurnState` seeded from the resolved profile.
    ///
    /// - Retry budgets (`schema_retries_remaining`, `done_nudges_remaining`)
    ///   come from `caps.output_schema_retries`.
    /// - `messages` and `turn_ctx` are left empty / `None`; the setup section
    ///   of `run_turn` populates them after the resume-detection branch.
    /// - `tools` is empty here; it is moved in by `run_turn` from
    ///   `caps.permissioned.into_parts()` immediately after `TurnState::new`,
    ///   so the `LoadContext` mutations live in the struct from iteration 0.
    pub fn new(caps: &ResolvedProfile) -> Self {
        Self {
            classify_denial_streak: 0,
            capability_dispatcher: Arc::new(super::capabilities::CapabilityDispatcher::new(
                std::iter::empty::<Arc<dyn Tool>>(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::iter::empty::<String>(),
                None,
                Vec::new(),
            )),
            total_usage: Usage::default(),
            clarify_signal: None,
            inform_signal: None,
            all_turn_text: String::new(),
            final_step_index: 0,
            messages: Vec::new(),
            turn_ctx: None,
            schema_retries_remaining: caps.output_schema_retries,
            done_nudges_remaining: caps.output_schema_retries,
            tools: Vec::new(),
            entries: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use gait_provider::provider::{Provider, ProviderError, StreamChunk};
    use std::sync::Arc;

    /// Minimal provider that never yields; required by `for_test`.
    struct NoopProvider;
    #[async_trait]
    impl Provider for NoopProvider {
        fn model_id(&self) -> &str {
            "noop"
        }
        async fn stream(
            &self,
            _system: &str,
            _messages: &[crate::types::Message],
            _tools: &[Arc<dyn Tool>],
            _params: &crate::config::LlmParamsConfig,
            _tail_context: Option<&str>,
        ) -> Result<
            std::pin::Pin<
                Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>,
            >,
            ProviderError,
        > {
            Err(ProviderError::Other("unused".into()))
        }
    }

    /// Task 2 / Step 1: `TurnState::new` seeds the retry budgets from
    /// `ResolvedProfile.output_schema_retries` and starts the message/entry
    /// vectors empty. The `tools` vector is empty here too — it is moved in
    /// by `run_turn` from `caps.permissioned` after construction.
    #[test]
    fn turn_state_new_seeds_retry_budgets_from_caps() {
        let provider: Arc<dyn Provider> = Arc::new(NoopProvider);
        let caps = ResolvedProfile::for_test(provider, vec![], vec![], vec![]);
        let st = TurnState::new(&caps);
        assert_eq!(st.schema_retries_remaining, 3);
        assert_eq!(st.done_nudges_remaining, 3);
        assert!(st.messages.is_empty());
        assert!(st.entries.is_empty());
        assert!(st.tools.is_empty());
        assert!(st.clarify_signal.is_none());
        assert!(st.inform_signal.is_none());
        assert!(st.turn_ctx.is_none());
        assert_eq!(st.final_step_index, 0);
        assert!(st.all_turn_text.is_empty());
    }
}
