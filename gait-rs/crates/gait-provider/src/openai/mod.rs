use crate::provider::{ChunkStream, Provider, ProviderError, StreamChunk};
use gait_types::types::{AssistantContent, AssistantPart, Message, MessageContent, Tool, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub struct OpenAIProvider {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    transport: crate::transport::SseTransport,
}

impl OpenAIProvider {
    /// Whether to request strict function calling. Explicit config wins; otherwise
    /// default on only for the real OpenAI API host (MiniMax and other compatible
    /// third-party hosts may not implement constrained decoding).
    fn resolve_strict(&self, cfg: Option<bool>) -> bool {
        cfg.unwrap_or_else(|| self.base_url.contains("api.openai.com"))
    }

    pub fn new(
        api_key: String,
        model: String,
        base_url: Option<String>,
        transport: crate::transport::SseTransport,
    ) -> Self {
        Self {
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
            transport,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
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
    reasoning_content: Option<String>,
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
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<u32>,
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
            Message::User {
                content,
                attribution,
                ..
            } => {
                let text = match content {
                    MessageContent::Text(t) => t.clone(),
                    MessageContent::Parts(parts) => parts
                        .iter()
                        .map(|p| match p {
                            gait_types::types::ContentPart::Text { text } => text.as_str(),
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                };
                let text = gait_types::types::render_user_text(attribution, &text);
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
        tools: &[Arc<dyn Tool>],
        params: &gait_types::config::LlmParamsConfig,
        tail_context: Option<&str>,
    ) -> Result<ChunkStream, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);

        let want_strict = self.resolve_strict(params.openai.as_ref().and_then(|o| o.strict_tools));
        let chat_tools: Vec<ChatTool> = tools
            .iter()
            .map(|t| {
                let base = t.parameters().clone();
                let (parameters, strict) = if want_strict {
                    match crate::strict::to_strict_schema(
                        &base,
                        crate::strict::StrictDialect::OpenAi,
                    ) {
                        Some(s) => (s, Some(true)),
                        None => (base, None),
                    }
                } else {
                    (base, None)
                };
                ChatTool {
                    r#type: "function".into(),
                    function: ChatToolDef {
                        name: t.name().to_string(),
                        description: gait_types::types::render_description(t.as_ref()),
                        parameters,
                        strict,
                    },
                }
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

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            crate::transport::sensitive_header(&format!("Bearer {}", self.api_key))?,
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        let request = crate::transport::SseRequest::new(url, headers, body_value);
        let stream = self.transport.stream(request).await?;
        Ok(Box::pin(body_stream(stream)))
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

/// Lazy SSE body parser for the OpenAI-compatible streaming shape.
/// Errors here are terminal — the connect/classification already happened upstream.
fn body_stream(
    guarded_events: impl futures_util::Stream<
            Item = Result<eventsource_stream::Event, crate::provider::ProviderError>,
        > + Send
        + 'static,
) -> impl futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send + 'static {
    use futures_util::StreamExt;
    async_stream::try_stream! {
        tokio::pin!(guarded_events);
        let mut tool_ids:   HashMap<u32, String> = HashMap::new();
        let mut tool_names: HashMap<u32, String> = HashMap::new();
        let mut tool_args:  HashMap<u32, String> = HashMap::new();

        let mut saw_terminal = false;
        let mut received_bytes: usize = 0;

        while let Some(event) = guarded_events.next().await {
            let event = match event {
                Ok(ev) => ev,
                // Connection dropped mid-body before [DONE] → silent-truncation
                // case; surface StreamIncomplete. Parse/Utf8 errors stay Sse.
                Err(e) => Err(e)?,
            };
            if event.data == "[DONE]" {
                if !saw_terminal {
                    yield StreamChunk::Done;
                }
                saw_terminal = true;
                break;
            }
            if event.data.is_empty() { continue; }

            let chunk: StreamChunkData = match serde_json::from_str(&event.data) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if let Some(usage) = chunk.usage {
                yield StreamChunk::StepUsage(Usage {
                    input_tokens: usage.prompt_tokens,
                    output_tokens: usage.completion_tokens,
                    cache_read_tokens: usage
                        .prompt_tokens_details
                        .and_then(|details| details.cached_tokens)
                        .unwrap_or(0),
                    cache_write_tokens: 0,
                });
            }

            for choice in &chunk.choices {
                if let Some(text) = &choice.delta.content {
                    if !text.is_empty() {
                        received_bytes += text.len();
                        yield StreamChunk::Text(text.clone());
                    }
                }

                if let Some(thinking) = &choice.delta.reasoning_content {
                    if !thinking.is_empty() {
                        yield StreamChunk::ThinkingDelta(thinking.clone());
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
                    saw_terminal = true;
                }
            }
        }

        if !saw_terminal {
            Err(ProviderError::StreamIncomplete { received_bytes })?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn openai_usage_deserializes_cached_prompt_tokens() {
        let chunk: StreamChunkData = serde_json::from_value(serde_json::json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 1200,
                "completion_tokens": 30,
                "prompt_tokens_details": {"cached_tokens": 900}
            }
        }))
        .unwrap();
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 1200);
        assert_eq!(usage.completion_tokens, 30);
        assert_eq!(
            usage
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens),
            Some(900)
        );
    }

    #[test]
    fn appending_messages_preserves_serialized_prefix() {
        let first = vec![
            Message::User {
                id: "u1".into(),
                content: MessageContent::Text("first".into()),
                attribution: Vec::new(),
            },
            Message::Assistant {
                id: "a1".into(),
                content: AssistantContent::Text("answer".into()),
            },
        ];
        let mut second = first.clone();
        second.push(Message::User {
            id: "u2".into(),
            content: MessageContent::Text("new event".into()),
            attribution: Vec::new(),
        });

        let first_wire = to_chat_messages("stable system", &first);
        let second_wire = to_chat_messages("stable system", &second);
        let first_json = serde_json::to_value(&first_wire).unwrap();
        let second_json = serde_json::to_value(&second_wire).unwrap();
        assert_eq!(
            &second_json.as_array().unwrap()[..first_json.as_array().unwrap().len()],
            first_json.as_array().unwrap().as_slice(),
            "ordinary log append must preserve the request prefix"
        );
    }

    #[test]
    fn openai_strict_tool_serializes_strict_and_nullable_optionals() {
        let base = serde_json::json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "integer"}},
            "required": ["a"]
        });
        let strict =
            crate::strict::to_strict_schema(&base, crate::strict::StrictDialect::OpenAi).unwrap();
        let def = ChatToolDef {
            name: "t".into(),
            description: "d".into(),
            parameters: strict,
            strict: Some(true),
        };
        let v = serde_json::to_value(&def).unwrap();
        assert_eq!(v["strict"], serde_json::json!(true));
        assert_eq!(
            v["parameters"]["additionalProperties"],
            serde_json::json!(false)
        );
        // optional `b` is now required + nullable
        let req: Vec<String> = v["parameters"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect();
        assert!(req.contains(&"b".to_string()));
        assert_eq!(
            v["parameters"]["properties"]["b"]["type"],
            serde_json::json!(["integer", "null"])
        );
    }

    #[test]
    fn openai_non_strict_tool_omits_strict() {
        let def = ChatToolDef {
            name: "t".into(),
            description: "d".into(),
            parameters: serde_json::json!({}),
            strict: None,
        };
        let v = serde_json::to_value(&def).unwrap();
        assert!(v.get("strict").is_none());
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

    #[test]
    fn to_chat_messages_wraps_user_message_in_room_message_tag() {
        let msg = Message::User {
            id: "test-msg-1".into(),
            content: MessageContent::Text("Hello, world!".into()),
            attribution: vec![gait_types::types::Attribution {
                author_agent_id: "agent-2".into(),
                room_id: "room-general".into(),
                trust: gait_types::types::Trust::Unverified,
            }],
        };
        let chat_msgs = to_chat_messages("", &[msg]);
        assert_eq!(chat_msgs.len(), 1);
        let content_str = chat_msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert_eq!(
            content_str,
            "<room-message from=\"agent-2\" room=\"room-general\" trust=\"unverified\">Hello, world!</room-message>\n"
        );
    }
}
