//! `TurnState` — the mutable loop-carried state of `run_turn()`.
//!
//! This is the State-half of the Stage-1 refactor in
//! `architecture-mapping/plan-01-run-turn-decomposition.md` (Task 2). It is a
//! pure mechanical hoist: the scattered `let mut` locals that span the
//! pre-loop setup and the main `for step_index in 0..max_iter` loop are
//! gathered here behind a single `let mut state = TurnState::new(&caps);`.
//!
//! **No behavior changes.** The Stage-0 golden-transcript suite
//! (`src/turn/golden_tests.rs` + `src/turn/snapshots/`) pins every exit kind;
//! a snapshot diff after this refactor indicates a behavior change and must be
//! fixed in the refactor — never in the snapshot.
//!
//! **Read-only borrows** (session, ingestor, emitter, provider, repos, hooks,
//! profiles, etc.) stay in `run_turn`'s frame and are folded into `TurnEnv`
//! in Task 3 — that work is *not* in this module.

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
    /// structured-done `turn_complete` branches via `split_into_blocks`.
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
    /// `schema_retries_remaining` budget on `ExecutionSignal::TurnComplete`).
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
    pub tools: Vec<Box<dyn Tool>>,

    /// `<loadable-index>` entries (MCP server / skill / plugin rows) built
    /// once in setup and read every step by
    /// `prompt::build_loadable_enabled_state`.
    pub entries: Vec<LoadableEntry>,
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
    use crate::provider::{Provider, ProviderError, StreamChunk};
    use async_trait::async_trait;
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
            _tools: &[Box<dyn Tool>],
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
