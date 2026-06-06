use crate::provider::{ProviderError, StreamChunk};
use crate::types::Usage;
use crate::anthropic::types::{AnthropicEvent, ContentBlock, ContentDelta};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use std::collections::HashMap;

pub struct SseClient {
    pub(crate) http: Client,
    pub(crate) api_key: String,
}

impl SseClient {
    pub fn new(api_key: String) -> Self {
        Self {
            http: Client::new(),
            api_key,
        }
    }

    /// Issue a streaming POST and return a 'static stream of chunks.
    /// Takes `self` by value so the returned stream is owned (not borrowing &self).
    pub fn stream_request(
        self,
        body: serde_json::Value,
    ) -> impl Stream<Item = Result<StreamChunk, ProviderError>> + Send + 'static {
        let http = self.http;
        let api_key = self.api_key;

        async_stream::try_stream! {
            let resp = http
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await?;

            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                Err(ProviderError::Http { status, body })?;
                return;
            }

            let mut tool_name_map: HashMap<u32, String> = HashMap::new();
            let mut tool_id_map: HashMap<u32, String> = HashMap::new();
            let mut args_buf: HashMap<u32, String> = HashMap::new();

            let mut stream = resp.bytes_stream().eventsource();

            while let Some(event) = stream.next().await {
                let event = event.map_err(|e| ProviderError::Sse(e.to_string()))?;
                if event.event == "message_stop" { break; }
                if event.data.is_empty() || event.data == "[DONE]" { continue; }

                let parsed: AnthropicEvent = serde_json::from_str(&event.data)?;

                match parsed {
                    // Emit input-token count from the opening message event (C5 fix)
                    AnthropicEvent::MessageStart { message } => {
                        if let Some(u) = message.usage {
                            yield StreamChunk::StepUsage(Usage {
                                input_tokens: u.input_tokens.unwrap_or(0),
                                output_tokens: 0,
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
                        }
                    }
                    AnthropicEvent::ContentBlockDelta { index, delta } => match delta {
                        ContentDelta::TextDelta { text } => {
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
                    },
                    AnthropicEvent::ContentBlockStop { index } => {
                        if let (Some(id), Some(name), Some(buf)) = (
                            tool_id_map.remove(&index),
                            tool_name_map.remove(&index),
                            args_buf.remove(&index),
                        ) {
                            let args = serde_json::from_str(&buf)
                                .unwrap_or(serde_json::Value::String(buf));
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
                        });
                    }
                    AnthropicEvent::MessageStop => { yield StreamChunk::Done; break; }
                    AnthropicEvent::Error { error } => {
                        Err(ProviderError::Sse(error.message))?;
                    }
                    _ => {}
                }
            }
        }
    }
}
