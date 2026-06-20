use crate::provider::{ChunkStream, Provider, ProviderError, StreamChunk};
use crate::types::{AssistantContent, AssistantPart, Message, MessageContent, Tool, Usage};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub struct OpenAIProvider {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
}

impl OpenAIProvider {
    pub fn new(api_key: String, model: String, base_url: Option<String>) -> Self {
        Self {
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
        }
    }
}

// ── Request types ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ChatTool>,
    stream: bool,
    stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize)]
struct ChatToolCall {
    id: String,
    r#type: String,
    function: ChatFunction,
}

#[derive(Serialize)]
struct ChatFunction {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct ChatTool {
    r#type: String,
    function: ChatToolDef,
}

#[derive(Serialize)]
struct ChatToolDef {
    name: String,
    description: String,
    parameters: Value,
}

// ── SSE response types ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StreamChunkData {
    choices: Vec<StreamChoice>,
    usage: Option<StreamUsage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Delta {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: u32,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct StreamUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

// ── Message conversion ────────────────────────────────────────────────────────

fn to_chat_messages(system: &str, messages: &[Message]) -> Vec<ChatMessage> {
    let mut out = vec![];
    if !system.is_empty() {
        out.push(ChatMessage {
            role: "system".into(),
            content: Some(Value::String(system.into())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }
    for m in messages {
        match m {
            Message::User { content, .. } => {
                let text = match content {
                    MessageContent::Text(t) => t.clone(),
                    MessageContent::Parts(parts) => parts
                        .iter()
                        .filter_map(|p| match p {
                            crate::types::ContentPart::Text { text } => Some(text.as_str()),
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                };
                out.push(ChatMessage {
                    role: "user".into(),
                    content: Some(Value::String(text)),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            Message::Assistant { content, .. } => {
                let (text, tool_calls) = match content {
                    AssistantContent::Text(t) => (Some(t.clone()), vec![]),
                    AssistantContent::Parts(parts) => {
                        let text = parts.iter().find_map(|p| {
                            if let AssistantPart::Text { text } = p {
                                Some(text.clone())
                            } else {
                                None
                            }
                        });
                        let tcs: Vec<_> = parts
                            .iter()
                            .filter_map(|p| {
                                if let AssistantPart::ToolCall(tc) = p {
                                    Some(ChatToolCall {
                                        id: tc.tool_call_id.clone(),
                                        r#type: "function".into(),
                                        function: ChatFunction {
                                            name: tc.tool_name.clone(),
                                            // Backstop: function arguments must serialize as a JSON
                                            // object. Coerce non-objects from older transcripts to `{}`.
                                            arguments: crate::provider::coerce_tool_input(
                                                tc.args.clone(),
                                            )
                                            .to_string(),
                                        },
                                    })
                                } else {
                                    None
                                }
                            })
                            .collect();
                        (text, tcs)
                    }
                };
                out.push(ChatMessage {
                    role: "assistant".into(),
                    content: text.map(Value::String),
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                    name: None,
                });
            }
            Message::ToolResult { content, .. } => {
                for tr in content {
                    out.push(ChatMessage {
                        role: "tool".into(),
                        content: Some(Value::String(tr.result.to_string())),
                        tool_calls: None,
                        tool_call_id: Some(tr.tool_call_id.clone()),
                        name: None,
                    });
                }
            }
        }
    }
    out
}

// ── Provider impl ─────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl Provider for OpenAIProvider {
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Box<dyn Tool>],
        params: &crate::config::LlmParamsConfig,
        tail_context: Option<&str>,
    ) -> Result<ChunkStream, ProviderError> {
        let http = Client::new();
        let url = format!("{}/chat/completions", self.base_url);
        let api_key = self.api_key.clone();

        let chat_tools: Vec<ChatTool> = tools
            .iter()
            .map(|t| ChatTool {
                r#type: "function".into(),
                function: ChatToolDef {
                    name: t.name().to_string(),
                    description: crate::types::render_description(t.as_ref()),
                    parameters: t.parameters().clone(),
                },
            })
            .collect();

        let openai_p = params.openai.as_ref();

        let body = ChatRequest {
            model: self.model.clone(),
            messages: {
                let mut msgs = to_chat_messages(system, messages);
                // Append volatile tail context as an additional user-role message.
                // OpenAI has no per-message cache breakpoints; the prefix cache
                // is still keyed on the prefix ordering, so the tail stays at the
                // end (uncached region) where churn is free.
                if let Some(tail) = tail_context {
                    msgs.push(ChatMessage {
                        role: "user".into(),
                        content: Some(Value::String(tail.to_string())),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    });
                }
                msgs
            },
            tools: chat_tools,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            temperature: params.temperature,
            top_p: params.top_p,
            max_completion_tokens: params.max_tokens,
            reasoning_effort: openai_p.and_then(|o| o.reasoning_effort.clone()),
            frequency_penalty: openai_p.and_then(|o| o.frequency_penalty),
            presence_penalty: openai_p.and_then(|o| o.presence_penalty),
            seed: openai_p.and_then(|o| o.seed),
            stop: openai_p.map(|o| o.stop.clone()).unwrap_or_default(),
        };

        let body_value = serde_json::to_value(&body)?;

        let stream = async_stream::try_stream! {
            let resp = http
                .post(&url)
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .json(&body_value)
                .send()
                .await?;

            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                Err(ProviderError::Http { status, body })?;
                return;
            }

            let mut tool_ids:   HashMap<u32, String> = HashMap::new();
            let mut tool_names: HashMap<u32, String> = HashMap::new();
            let mut tool_args:  HashMap<u32, String> = HashMap::new();

            let mut sse = resp.bytes_stream().eventsource();

            while let Some(event) = sse.next().await {
                let event = event.map_err(|e| ProviderError::Sse(e.to_string()))?;
                if event.data == "[DONE]" { break; }
                if event.data.is_empty() { continue; }

                let chunk: StreamChunkData = match serde_json::from_str(&event.data) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                if let Some(usage) = chunk.usage {
                    yield StreamChunk::StepUsage(Usage {
                        input_tokens: usage.prompt_tokens,
                        output_tokens: usage.completion_tokens,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    });
                }

                for choice in &chunk.choices {
                    if let Some(text) = &choice.delta.content {
                        if !text.is_empty() {
                            yield StreamChunk::Text(text.clone());
                        }
                    }

                    if let Some(tcs) = &choice.delta.tool_calls {
                        for tc in tcs {
                            let idx = tc.index;
                            if let Some(id) = &tc.id {
                                tool_ids.entry(idx).or_insert_with(|| id.clone());
                            }
                            if let Some(f) = &tc.function {
                                if let Some(name) = &f.name {
                                    tool_names.entry(idx).or_insert_with(|| name.clone());
                                }
                                if let Some(args) = &f.arguments {
                                    tool_args.entry(idx).or_default().push_str(args);
                                    let id   = tool_ids.get(&idx).cloned().unwrap_or_default();
                                    let name = tool_names.get(&idx).cloned().unwrap_or_default();
                                    yield StreamChunk::ToolCallDelta {
                                        tool_call_id: id,
                                        tool_name: name,
                                        args_delta: args.clone(),
                                    };
                                }
                            }
                        }
                    }

                    if matches!(choice.finish_reason.as_deref(), Some("tool_calls") | Some("stop")) {
                        for (idx, args_str) in &tool_args {
                            let id   = tool_ids.get(idx).cloned().unwrap_or_default();
                            let name = tool_names.get(idx).cloned().unwrap_or_default();
                            let args = crate::provider::parse_tool_args(args_str);
                            yield StreamChunk::ToolCallComplete { tool_call_id: id, tool_name: name, args };
                        }
                        yield StreamChunk::Done;
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_request_serializes_all_optional_params() {
        let req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![],
            tools: vec![],
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            temperature: Some(0.8),
            top_p: Some(0.95),
            max_completion_tokens: Some(2048),
            reasoning_effort: Some("high".into()),
            frequency_penalty: Some(0.1),
            presence_penalty: Some(0.2),
            seed: Some(42),
            stop: vec!["<|end|>".into()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["temperature"].as_f64().unwrap() as f32, 0.8_f32);
        assert_eq!(json["top_p"].as_f64().unwrap() as f32, 0.95_f32);
        assert_eq!(json["max_completion_tokens"], 2048);
        assert_eq!(json["reasoning_effort"], "high");
        assert_eq!(json["frequency_penalty"].as_f64().unwrap() as f32, 0.1_f32);
        assert_eq!(json["presence_penalty"].as_f64().unwrap() as f32, 0.2_f32);
        assert_eq!(json["seed"], 42);
        assert_eq!(json["stop"][0], "<|end|>");
    }

    #[test]
    fn chat_request_omits_none_fields() {
        let req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![],
            tools: vec![],
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            temperature: None,
            top_p: None,
            max_completion_tokens: None,
            reasoning_effort: None,
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            stop: vec![],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("temperature").is_none());
        assert!(json.get("top_p").is_none());
        assert!(json.get("max_completion_tokens").is_none());
        assert!(json.get("reasoning_effort").is_none());
        assert!(json.get("stop").is_none());
    }
}
