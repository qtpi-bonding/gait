use crate::anthropic::types::{AnthropicEvent, ContentBlock, ContentDelta};
use crate::provider::{ProviderError, StreamChunk};
use futures_util::{Stream, StreamExt};
use gait_types::types::Usage;
use std::collections::HashMap;

/// Consume the shared guarded SSE stream into Anthropic chunks. Request
/// execution, retries, and deadlines have already been handled by ProviderTransport.
pub(crate) fn body_stream(
    guarded_events: impl futures_util::Stream<
            Item = Result<eventsource_stream::Event, crate::provider::ProviderError>,
        > + Send
        + 'static,
    mut call: crate::telemetry::ModelCall,
) -> impl Stream<Item = Result<StreamChunk, ProviderError>> + Send + 'static {
    async_stream::try_stream! {
        tokio::pin!(guarded_events);
        let mut tool_name_map: HashMap<u32, String> = HashMap::new();
        let mut tool_id_map: HashMap<u32, String> = HashMap::new();
        let mut args_buf: HashMap<u32, String> = HashMap::new();
        let mut thinking_indices: std::collections::HashSet<u32> = std::collections::HashSet::new();

        let mut start_usage: (u32, u32, u32) = (0, 0, 0);
        let mut saw_terminal = false;
        let mut received_bytes: usize = 0;
        // Cumulative counts for the call span: input from message_start,
        // output (and any repeated input) from message_delta.
        let mut span_usage = crate::telemetry::CallUsage::default();

        while let Some(event) = guarded_events.next().await {
            let event = match event {
                Ok(ev) => ev,
                Err(e) => {
                    // already converted by the shared transport
                    call.fail(&e);
                    Err(e)?
                }
            };
            if event.event == "message_stop" {
                saw_terminal = true;
                call.answered();
                call.end();
                yield StreamChunk::Done;
                break;
            }
            if event.data.is_empty() || event.data == "[DONE]" { continue; }

            let parsed: AnthropicEvent = match serde_json::from_str(&event.data) {
                Ok(parsed) => parsed,
                Err(e) => {
                    let e = ProviderError::from(e);
                    call.fail(&e);
                    Err(e)?
                }
            };

            match parsed {
                // Emit input-token count from the opening message event (C5 fix)
                AnthropicEvent::MessageStart { message } => {
                    if let Some(model) = &message.model {
                        call.response_model(model);
                    }
                    if let Some(u) = message.usage {
                        start_usage = (
                            u.input_tokens.unwrap_or(0),
                            u.cache_read_input_tokens.unwrap_or(0),
                            u.cache_creation_input_tokens.unwrap_or(0),
                        );
                        span_usage.cached = start_usage.1;
                        span_usage.cache_write = start_usage.2;
                        span_usage.input = start_usage.0 + start_usage.1 + start_usage.2;
                        span_usage.output = u.output_tokens.unwrap_or(0);
                        call.usage(span_usage);
                        yield StreamChunk::StepUsage(Usage {
                            input_tokens: u.input_tokens.unwrap_or(0),
                            output_tokens: 0,
                            cache_read_tokens: u.cache_read_input_tokens.unwrap_or(0),
                            cache_write_tokens: u.cache_creation_input_tokens.unwrap_or(0),
                        });
                    }
                }
                AnthropicEvent::ContentBlockStart { index, content_block } => {
                    if let ContentBlock::ToolUse { id, name } = content_block {
                        call.first_token();
                        tool_id_map.insert(index, id.clone());
                        tool_name_map.insert(index, name.clone());
                        args_buf.insert(index, String::new());
                        yield StreamChunk::ToolCallDelta {
                            tool_call_id: id,
                            tool_name: name,
                            args_delta: String::new(),
                        };
                    } else if let ContentBlock::Thinking { .. } = content_block {
                        // Track which block indices are thinking blocks so that
                        // ThinkingDelta deltas can be routed out as StreamChunk::ThinkingDelta.
                        thinking_indices.insert(index);
                    }
                }
                AnthropicEvent::ContentBlockDelta { index, delta } => match delta {
                    ContentDelta::TextDelta { text } => {
                        received_bytes += text.len();
                        call.first_token();
                        yield StreamChunk::Text(text);
                    }
                    ContentDelta::InputJsonDelta { partial_json } => {
                        if let Some(buf) = args_buf.get_mut(&index) {
                            buf.push_str(&partial_json);
                            let id = tool_id_map.get(&index).cloned().unwrap_or_default();
                            let name = tool_name_map.get(&index).cloned().unwrap_or_default();
                            yield StreamChunk::ToolCallDelta {
                                tool_call_id: id,
                                tool_name: name,
                                args_delta: partial_json,
                            };
                        }
                    }
                    ContentDelta::ThinkingDelta { thinking } => {
                        // Always emit — show_thinking gate is in GaitConfig, not here.
                        if thinking_indices.contains(&index) {
                            call.first_token();
                            yield StreamChunk::ThinkingDelta(thinking);
                        }
                    }
                    ContentDelta::SignatureDelta { signature } => {
                        // Anthropic extended-thinking sends the signature in a
                        // signature_delta after the thinking content; emit it so
                        // turn/mod.rs can pair it with the accumulated text.
                        yield StreamChunk::ThinkingSignature { index, signature };
                    }
                    ContentDelta::Unknown => {}
                },
                AnthropicEvent::ContentBlockStop { index } => {
                    if let (Some(id), Some(name), Some(buf)) = (
                        tool_id_map.remove(&index),
                        tool_name_map.remove(&index),
                        args_buf.remove(&index),
                    ) {
                        let args = crate::provider::parse_tool_args(&buf);
                        yield StreamChunk::ToolCallComplete {
                            tool_call_id: id,
                            tool_name: name,
                            args,
                        };
                    }
                }
                AnthropicEvent::MessageDelta { delta, usage } => {
                    if let Some(reason) = delta.stop_reason.as_deref() {
                        call.finish_reason(reason);
                        call.answered();
                    }
                    let Some(u) = usage else { continue };
                    span_usage.cached = span_usage.cached.max(u.cache_read_input_tokens.unwrap_or(0));
                    span_usage.cache_write = span_usage.cache_write.max(u.cache_creation_input_tokens.unwrap_or(0));
                    span_usage.input = u.input_tokens.unwrap_or(0).max(start_usage.0)
                        + span_usage.cached
                        + span_usage.cache_write;
                    span_usage.output = u.output_tokens.unwrap_or(0);
                    call.usage(span_usage);
                    // message_delta usage is cumulative and may repeat the input
                    // and cache counts already taken from message_start; add
                    // only what exceeds them or they would be counted twice.
                    yield StreamChunk::StepUsage(Usage {
                        input_tokens: u.input_tokens.unwrap_or(0).saturating_sub(start_usage.0),
                        output_tokens: u.output_tokens.unwrap_or(0),
                        cache_read_tokens: u.cache_read_input_tokens.unwrap_or(0).saturating_sub(start_usage.1),
                        cache_write_tokens: u.cache_creation_input_tokens.unwrap_or(0).saturating_sub(start_usage.2),
                    });
                }
                AnthropicEvent::MessageStop => { saw_terminal = true; call.answered(); call.end(); yield StreamChunk::Done; break; }
                AnthropicEvent::Error { error } => {
                    let e = if error.kind == "overloaded_error" {
                        ProviderError::Overloaded
                    } else {
                        ProviderError::Sse(error.message)
                    };
                    call.fail(&e);
                    Err(e)?;
                }
                _ => {}
            }
        }

        if !saw_terminal {
            let e = ProviderError::StreamIncomplete { received_bytes };
            call.fail(&e);
            Err(e)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn ev(name: &str, data: serde_json::Value) -> Result<eventsource_stream::Event, ProviderError> {
        Ok(eventsource_stream::Event {
            event: name.into(),
            data: data.to_string(),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn a_message_delta_that_repeats_start_usage_is_not_counted_twice() {
        let events = futures_util::stream::iter(vec![
            ev("message_start", serde_json::json!({"type":"message_start","message":{"usage":{
                "input_tokens": 10, "cache_read_input_tokens": 900, "cache_creation_input_tokens": 50}}})),
            ev("message_delta", serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{
                "input_tokens": 10, "output_tokens": 7, "cache_read_input_tokens": 900, "cache_creation_input_tokens": 50}})),
            ev("message_stop", serde_json::json!({"type":"message_stop"})),
        ]);
        let mut total = Usage::default();
        let stream = body_stream(events, crate::telemetry::ModelCall::begin(&Default::default()));
        tokio::pin!(stream);
        while let Some(chunk) = stream.next().await {
            if let StreamChunk::StepUsage(u) = chunk.expect("chunk") {
                total = total.add(&u);
            }
        }
        assert_eq!(
            (total.input_tokens, total.output_tokens, total.cache_read_tokens, total.cache_write_tokens),
            (10, 7, 900, 50)
        );
    }
}
