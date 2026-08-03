use crate::anthropic::types::{AnthropicEvent, ContentBlock, ContentDelta};
use crate::provider::{ProviderError, StreamChunk};
use futures_util::{Stream, StreamExt};
use gait_types::types::Usage;
use std::collections::HashMap;

/// Consume the shared guarded SSE stream into Anthropic chunks. Request
/// execution, retries, and deadlines have already been handled by SseTransport.
pub fn body_stream(
    guarded_events: impl futures_util::Stream<
            Item = Result<eventsource_stream::Event, crate::provider::ProviderError>,
        > + Send
        + 'static,
) -> impl Stream<Item = Result<StreamChunk, ProviderError>> + Send + 'static {
    async_stream::try_stream! {
        tokio::pin!(guarded_events);
        let mut tool_name_map: HashMap<u32, String> = HashMap::new();
        let mut tool_id_map: HashMap<u32, String> = HashMap::new();
        let mut args_buf: HashMap<u32, String> = HashMap::new();
        let mut thinking_indices: std::collections::HashSet<u32> = std::collections::HashSet::new();

        let mut saw_terminal = false;
        let mut received_bytes: usize = 0;

        while let Some(event) = guarded_events.next().await {
            let event = match event {
                Ok(ev) => ev,
                Err(e) => Err(e)?,  // already converted by the shared transport
            };
            if event.event == "message_stop" {
                saw_terminal = true;
                yield StreamChunk::Done;
                break;
            }
            if event.data.is_empty() || event.data == "[DONE]" { continue; }

            let parsed: AnthropicEvent = serde_json::from_str(&event.data)?;

            match parsed {
                // Emit input-token count from the opening message event (C5 fix)
                AnthropicEvent::MessageStart { message } => {
                    if let Some(u) = message.usage {
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
                AnthropicEvent::MessageDelta { usage: Some(u), .. } => {
                    yield StreamChunk::StepUsage(Usage {
                        input_tokens: u.input_tokens.unwrap_or(0),
                        output_tokens: u.output_tokens.unwrap_or(0),
                        cache_read_tokens: u.cache_read_input_tokens.unwrap_or(0),
                        cache_write_tokens: u.cache_creation_input_tokens.unwrap_or(0),
                    });
                }
                AnthropicEvent::MessageStop => { saw_terminal = true; yield StreamChunk::Done; break; }
                AnthropicEvent::Error { error } => {
                    if error.kind == "overloaded_error" {
                        Err(ProviderError::Overloaded)?;
                    } else {
                        Err(ProviderError::Sse(error.message))?;
                    }
                }
                _ => {}
            }
        }

        if !saw_terminal {
            Err(ProviderError::StreamIncomplete { received_bytes })?;
        }
    }
}
