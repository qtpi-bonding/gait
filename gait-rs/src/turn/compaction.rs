//! Automatic, provider-neutral context compaction at the pre-sampling seam.
//!
//! This module never rewrites history or reads full-output files.  It measures
//! the model-facing projection, obtains one no-tools continuation checkpoint,
//! and lets the caller append the resulting session-scoped `Summary` event.

use futures_util::StreamExt;
use std::sync::Arc;

use crate::profile::ContextPolicy;
use crate::types::{AssistantContent, AssistantPart, Message, Usage};
use gait_provider::provider::{RequestKind, StreamChunk};

/// Conservative replacement for a provider tokenizer.  Kept here so it can
/// be replaced in one place when a provider-neutral tokenizer becomes viable.
pub const ESTIMATED_CHARS_PER_TOKEN: u64 = 3;
/// Total output budget for one compaction checkpoint. This deliberately leaves
/// room for reasoning tokens on providers whose generation limit includes
/// hidden reasoning as well as visible text.
pub const SUMMARY_OUTPUT_BUDGET_TOKENS: u64 = 8_192;
/// Harness-owned full-output references are allocated independently so they
/// can never make an otherwise valid model response fail validation.
pub const FULL_OUTPUT_INDEX_BUDGET_TOKENS: u64 = 256;
pub const SUMMARY_MODEL_OUTPUT_BUDGET_TOKENS: u64 =
    SUMMARY_OUTPUT_BUDGET_TOKENS - FULL_OUTPUT_INDEX_BUDGET_TOKENS;
/// Framing uncertainty which is not represented by Gait's serializable input.
pub const PROVIDER_FRAMING_RESERVE_TOKENS: u64 = 512;
pub const FULL_OUTPUT_INDEX_MAX_ENTRIES: usize = 32;

const COMPACTION_INSTRUCTION: &str = "Summarize this session into compact continuation state. Preserve the user's goal and constraints, decisions and reasons, current plan and remaining work, files and symbols touched, test status, important tool findings, archived full-output references, and blockers. Be factual; do not claim completion that the history does not show.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextEstimate {
    pub observed_tokens: u64,
    pub appended_estimate_tokens: u64,
    pub reserved_tokens: u64,
}

impl ContextEstimate {
    pub fn predicted_tokens(self) -> u64 {
        self.observed_tokens
            .saturating_add(self.appended_estimate_tokens)
            .saturating_add(self.reserved_tokens)
    }

    pub fn crosses(self, policy: ContextPolicy) -> bool {
        self.predicted_tokens() >= policy.compaction_threshold_tokens
    }
}

/// The deterministic context meter.  Measuring the complete outgoing request
/// is intentionally conservative; provider usage remains useful diagnostics,
/// but is never allowed to make a request look smaller than it is.
pub fn estimate(
    messages: &[Message],
    system_prompt: &str,
    tools: &[Arc<dyn crate::types::Tool>],
    session_tail: &str,
    observed_tokens: u64,
) -> ContextEstimate {
    let messages_bytes = serde_json::to_vec(messages)
        .map(|v| v.len())
        .unwrap_or_default();
    let tool_bytes: usize = tools
        .iter()
        .map(|tool| {
            tool.name().len()
                + tool.description().len()
                + serde_json::to_vec(tool.parameters())
                    .map(|v| v.len())
                    .unwrap_or_default()
        })
        .sum();
    let visible_bytes = messages_bytes
        .saturating_add(system_prompt.len())
        .saturating_add(tool_bytes)
        .saturating_add(session_tail.len()) as u64;
    let full_estimate = visible_bytes.div_ceil(ESTIMATED_CHARS_PER_TOKEN);
    ContextEstimate {
        observed_tokens,
        appended_estimate_tokens: full_estimate.saturating_sub(observed_tokens),
        reserved_tokens: PROVIDER_FRAMING_RESERVE_TOKENS,
    }
}

pub fn observed_usage_after_latest_summary(events: &[crate::proto::gait::log::v1::Event]) -> u64 {
    use crate::proto::gait::log::v1::{event, payload};
    events
        .iter()
        .rev()
        .find_map(|event| match &event.body {
            Some(event::Body::Payload(payload)) => match &payload.kind {
                Some(payload::Kind::LlmCallCompleted(complete)) => {
                    complete.usage.as_ref().map(|u| {
                        u.input
                            .saturating_add(u.cache_read)
                            .saturating_add(u.cache_write)
                    })
                }
                Some(payload::Kind::Summary(_)) => Some(0),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or(0)
}

pub fn has_unmatched_tool_call(messages: &[Message]) -> bool {
    let mut calls = std::collections::HashSet::new();
    let mut results = std::collections::HashSet::new();
    for message in messages {
        match message {
            Message::Assistant {
                content: AssistantContent::Parts(parts),
                ..
            } => {
                for part in parts {
                    if let AssistantPart::ToolCall(call) = part {
                        calls.insert(&call.tool_call_id);
                    }
                }
            }
            Message::ToolResult { content, .. } => {
                for result in content {
                    results.insert(&result.tool_call_id);
                }
            }
            _ => {}
        }
    }
    calls != results
}

#[derive(Debug)]
pub enum Error {
    UnsafePendingToolCall,
    Cancelled,
    Provider(anyhow::Error),
    EmptyResponse,
    SummaryTooLarge,
    StillTooLarge { predicted: u64, threshold: u64 },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsafePendingToolCall => write!(f, "compaction unsafe: unmatched tool call"),
            Self::Cancelled => write!(f, "compaction cancelled"),
            Self::Provider(error) => write!(f, "compaction provider failed: {error}"),
            Self::EmptyResponse => write!(f, "compaction returned an empty response"),
            Self::SummaryTooLarge => write!(f, "compaction response exceeded its output budget"),
            Self::StillTooLarge { predicted, threshold } => write!(
                f,
                "compacted context still too large: predicted {predicted} tokens, threshold {threshold}"
            ),
        }
    }
}
impl std::error::Error for Error {}

pub struct SummaryResponse {
    pub summary: String,
    pub usage: Usage,
}

fn full_output_index(messages: &[Message], gait_dir: &std::path::Path, session_id: &str) -> String {
    let mut entries = Vec::new();
    let mut omitted = 0usize;
    for message in messages.iter().rev() {
        let Message::ToolResult { content, .. } = message else {
            continue;
        };
        for result in content.iter().rev() {
            let Some(text) = result.result.as_str() else {
                continue;
            };
            if let Some(full_output) =
                crate::tools::full_output_ref_from_text(text, gait_dir, session_id)
            {
                let entry = format!(
                    "  {}  {} ({} bytes)",
                    result.tool_call_id,
                    full_output.path.display(),
                    full_output.bytes
                );
                let candidate = format!(
                    "\n\nArchived tool output from compacted history:\n{}",
                    entries
                        .iter()
                        .chain(std::iter::once(&entry))
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                if entries.len() < FULL_OUTPUT_INDEX_MAX_ENTRIES
                    && (candidate.len() as u64).div_ceil(ESTIMATED_CHARS_PER_TOKEN)
                        <= FULL_OUTPUT_INDEX_BUDGET_TOKENS
                {
                    entries.push(entry);
                } else {
                    omitted += 1;
                }
            }
        }
    }
    if entries.is_empty() {
        return String::new();
    }
    let mut index = format!(
        "\n\nArchived tool output from compacted history:\n{}",
        entries.join("\n")
    );
    if omitted > 0 {
        index.push_str(&format!("\n  … {omitted} older archived outputs omitted"));
    }
    index
}

pub async fn summarize(
    env: &super::state::TurnEnv<'_>,
    messages: &[Message],
) -> Result<SummaryResponse, Error> {
    if env.cancel.is_some_and(|cancel| cancel.is_cancelled()) {
        return Err(Error::Cancelled);
    }
    let mut request = messages.to_vec();
    request.push(Message::User {
        content: crate::types::MessageContent::Text(COMPACTION_INSTRUCTION.to_string()),
        id: "compaction-instruction".to_string(),
        attribution: Vec::new(),
    });
    let no_tools: &[Arc<dyn crate::types::Tool>] = &[];
    let mut params = env.llm_params.clone();
    params.max_tokens = Some(
        params
            .max_tokens
            .unwrap_or(SUMMARY_MODEL_OUTPUT_BUDGET_TOKENS as u32)
            .min(SUMMARY_MODEL_OUTPUT_BUDGET_TOKENS as u32),
    );
    let stream = env
        .provider
        .stream_with_kind(
            env.system_prompt,
            &request,
            no_tools,
            &params,
            None,
            RequestKind::Compaction,
        )
        .await
        .map_err(|error| Error::Provider(anyhow::anyhow!(error)))?;
    futures_util::pin_mut!(stream);
    let mut text = String::new();
    let mut usage = Usage::default();
    while let Some(chunk) = stream.next().await {
        if env.cancel.is_some_and(|cancel| cancel.is_cancelled()) {
            return Err(Error::Cancelled);
        }
        match chunk.map_err(|error| Error::Provider(anyhow::anyhow!(error)))? {
            StreamChunk::Text(delta) => text.push_str(&delta),
            StreamChunk::StepUsage(delta) => usage = usage.add(&delta),
            StreamChunk::ToolCallComplete { .. } | StreamChunk::ToolCallDelta { .. } => {
                return Err(Error::Provider(anyhow::anyhow!(
                    "compaction unexpectedly returned a tool call"
                )));
            }
            StreamChunk::Done
            | StreamChunk::ThinkingDelta(_)
            | StreamChunk::ThinkingSignature { .. } => {}
        }
    }
    if text.trim().is_empty() {
        return Err(Error::EmptyResponse);
    }
    if (text.len() as u64).div_ceil(ESTIMATED_CHARS_PER_TOKEN) > SUMMARY_MODEL_OUTPUT_BUDGET_TOKENS
    {
        return Err(Error::SummaryTooLarge);
    }
    let summary = format!(
        "{}{}",
        text.trim(),
        full_output_index(messages, env.gait_dir, env.session_id)
    );
    if (summary.len() as u64).div_ceil(ESTIMATED_CHARS_PER_TOKEN) > SUMMARY_OUTPUT_BUDGET_TOKENS {
        return Err(Error::SummaryTooLarge);
    }
    Ok(SummaryResponse { summary, usage })
}

/// A single retry prevents a transient provider overload from discarding a
/// long-lived turn at the exact boundary where compaction is most valuable.
pub async fn summarize_with_retry(
    env: &super::state::TurnEnv<'_>,
    messages: &[Message],
) -> Result<SummaryResponse, Error> {
    match summarize(env, messages).await {
        Err(Error::Provider(_)) if !env.cancel.is_some_and(|cancel| cancel.is_cancelled()) => {
            summarize(env, messages).await
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantPart, ToolCall, ToolResult, ToolResultKind};

    #[test]
    fn threshold_equality_compacts_and_one_token_below_does_not() {
        let policy = ContextPolicy {
            compaction_threshold_tokens: 80,
        };
        assert!(ContextEstimate {
            observed_tokens: 80,
            appended_estimate_tokens: 0,
            reserved_tokens: 0
        }
        .crosses(policy));
        assert!(!ContextEstimate {
            observed_tokens: 79,
            appended_estimate_tokens: 0,
            reserved_tokens: 0
        }
        .crosses(policy));
    }

    #[test]
    fn pairing_guard_rejects_a_dangling_call() {
        let call = ToolCall {
            tool_call_id: "call".into(),
            tool_name: "read".into(),
            args: serde_json::Value::Null,
        };
        let call_message = Message::Assistant {
            content: AssistantContent::Parts(vec![AssistantPart::ToolCall(call.clone())]),
            id: "1".into(),
        };
        assert!(has_unmatched_tool_call(std::slice::from_ref(&call_message)));
        let result = ToolResult {
            kind: ToolResultKind::ToolResult,
            tool_call_id: call.tool_call_id,
            result: serde_json::Value::String("ok".into()),
        };
        assert!(!has_unmatched_tool_call(&[
            call_message,
            Message::ToolResult {
                content: vec![result],
                id: "2".into()
            }
        ]));
    }
}
