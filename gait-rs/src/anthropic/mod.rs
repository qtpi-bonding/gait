pub mod sse;
pub mod types;

use crate::anthropic::sse::SseClient;
use crate::anthropic::types::{
    to_anthropic_messages, AnthropicBlock, AnthropicContent, AnthropicOutputConfig,
    AnthropicRequest, AnthropicSystem, AnthropicSystemBlock, AnthropicThinkingRequest,
    AnthropicTool, CacheControl,
};
use crate::provider::{ChunkStream, Provider, ProviderError};
use crate::types::{Message, Tool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnthropicModelGen {
    /// Opus 4.5 and older: temperature OK, budget_tokens OK
    Legacy,
    /// Opus 4.6, Sonnet 4.6: temperature OK, budget_tokens deprecated
    Claude46,
    /// Opus 4.7, 4.8: NO temperature/top_p/top_k; no budget_tokens; disabled OK
    Opus47Plus,
    /// Fable 5, Mythos 5: NO temperature; adaptive ONLY; disabled returns 400
    FableFamily,
}

fn anthropic_model_gen(model: &str) -> AnthropicModelGen {
    if model.contains("fable") || model.contains("mythos") {
        AnthropicModelGen::FableFamily
    } else if model.contains("opus-4-8") || model.contains("opus-4-7") {
        AnthropicModelGen::Opus47Plus
    } else if model.contains("opus-4-6") || model.contains("sonnet-4-6") {
        AnthropicModelGen::Claude46
    } else {
        AnthropicModelGen::Legacy
    }
}

pub struct AnthropicProvider {
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
    pub cache: bool,
    http: reqwest::Client,
    transport: crate::transport::TransportConfig,
    sse: SseClient,
}

impl AnthropicProvider {
    pub fn new(
        api_key: String,
        model: String,
        base_url: Option<String>,
        cache: bool,
        transport: crate::transport::TransportConfig,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(transport.connect_timeout)
            .build()
            .expect("reqwest client");
        let resolved_base = base_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com/v1".into());
        let sse = SseClient {
            http: http.clone(),
            api_key: api_key.clone(),
            base_url: resolved_base,
        };
        Self {
            api_key,
            model,
            base_url,
            cache,
            http,
            transport,
            sse,
        }
    }
}

/// Assemble the Anthropic message array: convert messages, place the rolling
/// history cache breakpoint on the last *real* content block, then append the
/// volatile `tail_context` as an **uncached** trailing block. Pure and
/// side-effect-free — extracted from `stream` so cache-prefix placement is
/// unit-testable without a network call. The tail must never carry
/// `cache_control`, or volatile tail churn would bust the cached prefix.
fn build_anthropic_messages(
    messages: &[Message],
    cache: bool,
    tail_context: Option<&str>,
) -> Vec<crate::anthropic::types::AnthropicMessage> {
    let mut anthro_messages = to_anthropic_messages(messages);

    // Cache the full conversation history up to the current call.
    // Only the LLM's next response is "new" — everything already in messages is stable.
    // Mark the last content block of the last message.
    if cache {
        if let Some(last_msg) = anthro_messages.last_mut() {
            match &mut last_msg.content {
                AnthropicContent::Text(text) => {
                    let t = std::mem::take(text);
                    last_msg.content = AnthropicContent::Blocks(vec![AnthropicBlock::Text {
                        text: t,
                        cache_control: Some(CacheControl::ephemeral()),
                    }]);
                }
                AnthropicContent::Blocks(blocks) => {
                    if let Some(last) = blocks.last_mut() {
                        *last = std::mem::replace(
                            last,
                            AnthropicBlock::Text {
                                text: String::new(),
                                cache_control: None,
                            },
                        )
                        .with_cache();
                    }
                }
            }
        }
    }

    // Append the volatile tail context as an uncached content block on the
    // last real message. Cache breakpoint stays on the earlier block; the
    // tail carries no cache_control, so the cached prefix above it is
    // preserved across tail-only churn.
    if let Some(tail) = tail_context {
        if let Some(last_msg) = anthro_messages.last_mut() {
            let tail_block = AnthropicBlock::Text {
                text: tail.to_string(),
                cache_control: None,
            };
            match &mut last_msg.content {
                AnthropicContent::Text(existing) => {
                    // Replace the bare-text variant with a Blocks vec [original_text, tail].
                    let prev = std::mem::take(existing);
                    last_msg.content = AnthropicContent::Blocks(vec![
                        AnthropicBlock::Text {
                            text: prev,
                            cache_control: None,
                        },
                        tail_block,
                    ]);
                }
                AnthropicContent::Blocks(blocks) => {
                    blocks.push(tail_block);
                }
            }
        }
    }

    anthro_messages
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Box<dyn Tool>],
        params: &crate::config::LlmParamsConfig,
        tail_context: Option<&str>,
    ) -> Result<ChunkStream, ProviderError> {
        let system_field = if system.is_empty() {
            None
        } else if self.cache {
            Some(AnthropicSystem::Blocks(vec![
                AnthropicSystemBlock::text_cached(system),
            ]))
        } else {
            Some(AnthropicSystem::Text(system.into()))
        };

        let mut oai_tools: Vec<AnthropicTool> = tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name().to_string(),
                description: crate::types::render_description(t.as_ref()),
                input_schema: t.parameters().clone(),
                cache_control: None,
            })
            .collect();

        // Mark last tool with cache_control so the system+tools prefix is cached as one block.
        if self.cache && !oai_tools.is_empty() {
            let last = oai_tools
                .last_mut()
                .expect("guarded by !is_empty() check above");
            last.cache_control = Some(CacheControl::ephemeral());
        }

        let anthro_messages = build_anthropic_messages(messages, self.cache, tail_context);

        // Build optional param fields, respecting model-generation constraints.
        // Opus 4.7+/Fable: no temperature/top_p/top_k (400 error).
        let gen = anthropic_model_gen(&self.model);
        let temperature = match gen {
            AnthropicModelGen::Opus47Plus | AnthropicModelGen::FableFamily => None,
            _ => params.temperature,
        };
        let top_p = match gen {
            AnthropicModelGen::Opus47Plus | AnthropicModelGen::FableFamily => None,
            _ => params.top_p,
        };
        let top_k = match gen {
            AnthropicModelGen::Opus47Plus | AnthropicModelGen::FableFamily => None,
            _ => params.anthropic.as_ref().and_then(|a| a.top_k),
        };

        // Build thinking config from anthropic params. Fable/Mythos: "disabled" returns 400;
        // drop the whole thinking block to avoid the error.
        let thinking = params
            .anthropic
            .as_ref()
            .and_then(|a| a.thinking.as_ref())
            .and_then(|t| {
                let kind_str = t.kind.as_deref().unwrap_or("adaptive");
                if matches!(gen, AnthropicModelGen::FableFamily) && kind_str == "disabled" {
                    return None;
                }
                Some(AnthropicThinkingRequest {
                    kind: kind_str.to_string(),
                    budget_tokens: match gen {
                        AnthropicModelGen::Opus47Plus | AnthropicModelGen::FableFamily => None,
                        _ => t.budget_tokens,
                    },
                    display: t.display.clone(),
                })
            });

        // output_config carries effort for extended-thinking models.
        let output_config = params
            .anthropic
            .as_ref()
            .and_then(|a| a.effort.as_ref())
            .map(|effort| AnthropicOutputConfig {
                effort: Some(effort.clone()),
            });

        let max_tokens = params.max_tokens.unwrap_or(8096);

        let req = AnthropicRequest {
            model: self.model.clone(),
            max_tokens,
            system: system_field,
            messages: anthro_messages,
            tools: oai_tools,
            stream: true,
            temperature,
            top_p,
            top_k,
            thinking,
            output_config,
        };
        let body = serde_json::to_value(&req)?;
        // Pre-first-byte retry (Task 5): wrap the eager connect so a transient
        // 429/5xx/Connect/Timeout is retried with full-jitter backoff before the
        // first byte ever arrives. Cancel-continuity lives at the call site in
        // turn/mod.rs (the outer select on `cancel_token_fut(&cancel)`), per the
        // plan's v1 note; here we pass a fresh placeholder token.
        let resp = crate::transport::retry_phase1(
            &self.transport,
            &tokio_util::sync::CancellationToken::new(),
            || self.sse.connect(&body),
        )
        .await?;
        let stream = crate::transport::guard_idle(SseClient::body_stream(resp), self.transport);
        Ok(Box::pin(stream))
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod cache_prefix_tests {
    //! Gating acceptance test for the cache-prefix volatile-tail refactor.
    //!
    //! Guards the silent-regression failure mode: a misplaced cache breakpoint
    //! (or a tail that accidentally carries `cache_control`) still produces
    //! correct output, it just stops the prompt cache from hitting — no other
    //! test catches it.
    //!
    //! The meaningful invariant is **tail isolation**: with identical history,
    //! changing only the volatile tail must not perturb the cached prefix. (A
    //! naive byte-compare across a *growing* history would spuriously fail
    //! because the rolling breakpoint marker moves to the new last message —
    //! a move that is irrelevant to Anthropic's content-based prefix match at
    //! read time.)
    use super::*;
    use crate::types::{Message, MessageContent};

    fn history() -> Vec<Message> {
        vec![
            Message::User {
                content: MessageContent::Text("find the bug".into()),
                id: "u1".into(),
            },
            Message::User {
                content: MessageContent::Text("any update?".into()),
                id: "u2".into(),
            },
        ]
    }

    #[test]
    fn tail_block_is_uncached_and_breakpoint_stays_on_real_block() {
        let msgs = build_anthropic_messages(&history(), true, Some("STATUS A"));
        let blocks = match &msgs.last().expect("non-empty").content {
            AnthropicContent::Blocks(b) => b,
            AnthropicContent::Text(_) => panic!("expected Blocks on last message"),
        };
        assert!(blocks.len() >= 2, "expected real block + tail block");

        // The tail is the final block: it carries the volatile text and NO cache_control.
        match blocks.last().unwrap() {
            AnthropicBlock::Text {
                text,
                cache_control,
            } => {
                assert_eq!(text, "STATUS A");
                assert!(cache_control.is_none(), "tail block must NOT be cached");
            }
            _ => panic!("expected Text tail block"),
        }
        // The block before the tail is the real content and keeps the breakpoint.
        match &blocks[blocks.len() - 2] {
            AnthropicBlock::Text { cache_control, .. } => {
                assert!(
                    cache_control.is_some(),
                    "breakpoint must stay on the real block, not the tail"
                );
            }
            _ => panic!("expected Text real block before the tail"),
        }
    }

    #[test]
    fn changing_only_the_tail_does_not_perturb_the_cached_prefix() {
        // Full requests differ — the tail actually changed.
        let a = build_anthropic_messages(&history(), true, Some("STATUS A"));
        let b = build_anthropic_messages(&history(), true, Some("STATUS B"));
        assert_ne!(
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&b).unwrap(),
            "sanity: differing tails should produce differing requests"
        );

        // Build, then strip the trailing tail block, and serialize the remaining
        // cached prefix. `build_anthropic_messages` is pure, so we rebuild rather
        // than clone (AnthropicMessage is not Clone).
        let prefix = |tail: Option<&str>| {
            let mut msgs = build_anthropic_messages(&history(), true, tail);
            if tail.is_some() {
                if let Some(last) = msgs.last_mut() {
                    if let AnthropicContent::Blocks(blocks) = &mut last.content {
                        blocks.pop(); // drop the appended tail block
                    }
                }
            }
            serde_json::to_value(&msgs).unwrap()
        };

        // Tail-only churn leaves the cached prefix byte-identical...
        assert_eq!(
            prefix(Some("STATUS A")),
            prefix(Some("STATUS B")),
            "cached prefix diverged on tail-only churn — breakpoint regression"
        );
        // ...and the prefix equals the no-tail assembly (tail is purely additive).
        assert_eq!(
            prefix(Some("STATUS A")),
            prefix(None),
            "stripping the tail must recover the no-tail prefix"
        );
    }
}
