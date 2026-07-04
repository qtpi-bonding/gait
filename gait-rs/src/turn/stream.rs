//! One LLM streaming step — Stage 4 / Task 16.
//!
//! Owns the eager-connect + chunk-accumulation portion of the step loop:
//! calls `provider.stream(...)`, races the connect against cancellation,
//! then drains the resulting chunk stream into a [`StepYield`].
//!
//! **Risk (per plan-01 §Task 16, preserved exactly):** does not reorder
//! message-reconstruction relative to the caller (that already happened
//! before this is called), and does not move any `.await` across a
//! cancellation check — the two `tokio::select!` cancel races (connect, and
//! per-chunk) are reproduced verbatim, in the same order, with the same
//! `biased;` priority.
//!
//! Every failure mode here (cancel-during-connect, all four `ProviderError`
//! stream-error variants, and the generic stream-error catch-all) collapses
//! to `Err(anyhow::Error)` — the caller converts that into
//! `StepOutcome::Fail` uniformly, matching Task 12's Fail-path behavior.
//! The *separate* post-stream cancellation check (Esc pressed while the
//! step's tool-call events were still being processed) is NOT this
//! function's concern — the driver checks that itself, immediately after
//! calling `run_step`, since it must produce `StepOutcome::Cancelled`, not
//! `Fail`.

use futures_util::StreamExt;

use crate::types::{Message, ToolCall};
use gait_provider::provider::StreamChunk;

use super::state::{TurnEnv, TurnState};

/// Resolves when the cancellation token is cancelled; never resolves if no token.
async fn cancel_token_fut(token: Option<&tokio_util::sync::CancellationToken>) {
    match token {
        Some(t) => t.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// Everything one step's LLM stream produced, for the driver to hand to the
/// tool-execution phase.
pub(crate) struct StepYield {
    pub step_text: String,
    pub step_tool_calls: Vec<ToolCall>,
    /// Model text that arrived *after* a tool call completed mid-stream —
    /// flushed as deltas once the tool-execution phase is done with this step.
    pub post_tool_text: Vec<String>,
    pub step_thinking_text: String,
    pub step_thinking_signature: String,
}

/// Runs one LLM streaming step: eager-connect (racing cancellation), then
/// drains chunks until `StreamChunk::Done` or the stream closes.
///
/// `state.total_usage` is updated in place as `StepUsage` chunks arrive —
/// this is the one piece of `TurnState` this phase mutates directly rather
/// than returning via `StepYield`.
pub(crate) async fn run_step(
    env: &TurnEnv<'_>,
    state: &mut TurnState,
    outgoing: &[Message],
    session_state_body: &str,
) -> anyhow::Result<StepYield> {
    // The eager-connect phase (Task 3): this `.await` performs the HTTP POST and
    // returns a classified `ProviderError` if anything failed before the first
    // byte. We must still Esc-cancel from here, so wrap it in the same
    // cancel-select the loop below uses.
    let stream_fut = env.provider.stream(
        env.system_prompt,
        outgoing,
        &state.tools,
        env.llm_params,
        Some(session_state_body),
    );
    let mut chunk_stream = tokio::select! {
        biased;
        _ = cancel_token_fut(env.cancel) => {
            return Err(anyhow::anyhow!("provider cancelled during connect"));
        }
        res = stream_fut => res.map_err(|e| anyhow::anyhow!("provider error: {}", e))?,
    };

    let mut step_text = String::new();
    let mut step_tool_calls: Vec<ToolCall> = vec![];
    let mut post_tool_text: Vec<String> = vec![];
    let mut seen_tool_complete = false;
    let mut seen_first_content = false;
    let mut step_thinking_text = String::new();
    let mut step_thinking_signature = String::new();
    loop {
        let chunk_opt = tokio::select! {
            biased;
            _ = cancel_token_fut(env.cancel) => None,
            chunk = chunk_stream.next()   => chunk,
        };
        let Some(chunk_result) = chunk_opt else { break };
        let chunk = match chunk_result {
            Ok(c) => c,
            Err(gait_provider::provider::ProviderError::StreamIncomplete { received_bytes }) => {
                // Task 6: transport_error observability — v1 uses log::warn!
                // A structured recorder event can follow once transport errors
                // are promoted to first-class proto events.
                eprintln!(
                    "[gait] transport_error kind=stream_incomplete received_bytes={received_bytes}"
                );
                return Err(anyhow::anyhow!(
                    "stream incomplete after {received_bytes} bytes"
                ));
            }
            Err(gait_provider::provider::ProviderError::Timeout { phase }) => {
                eprintln!("[gait] transport_error kind=timeout phase={phase:?}");
                return Err(anyhow::anyhow!("stream timeout in phase {phase:?}"));
            }
            Err(gait_provider::provider::ProviderError::Overloaded) => {
                eprintln!("[gait] transport_error kind=overloaded");
                return Err(anyhow::anyhow!("provider overloaded"));
            }
            Err(e) => return Err(anyhow::anyhow!("stream error: {}", e)),
        };
        match chunk {
            StreamChunk::Text(t) => {
                if !seen_first_content {
                    crate::diagnostic::set_phase(crate::diagnostic::Phase::Streaming);
                    seen_first_content = true;
                }
                if seen_tool_complete {
                    post_tool_text.push(t.clone());
                    env.emitter.publish_delta(t.clone());
                }
                step_text.push_str(&t);
            }

            StreamChunk::ToolCallComplete {
                tool_call_id,
                tool_name,
                args,
            } => {
                seen_tool_complete = true;
                step_tool_calls.push(ToolCall {
                    tool_call_id,
                    tool_name,
                    args,
                });
            }
            StreamChunk::StepUsage(u) => {
                state.total_usage = state.total_usage.add(&u);
            }
            StreamChunk::Done => break,
            StreamChunk::ThinkingDelta(t) => {
                crate::diagnostic::set_phase(crate::diagnostic::Phase::Reasoning);
                step_thinking_text.push_str(&t);
            }
            StreamChunk::ThinkingSignature { signature, .. } => {
                step_thinking_signature = signature;
            }
            StreamChunk::ToolCallDelta { .. } => {}
        }
    }

    Ok(StepYield {
        step_text,
        step_tool_calls,
        post_tool_text,
        step_thinking_text,
        step_thinking_signature,
    })
}
