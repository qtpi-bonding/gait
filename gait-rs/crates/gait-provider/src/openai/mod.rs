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
    transport: crate::transport::ProviderTransport,
    prompt_cache_key: Option<String>,
    session_header: Option<String>,
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
        transport: crate::transport::ProviderTransport,
    ) -> Self {
        Self {
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
            transport,
            prompt_cache_key: None,
            session_header: None,
        }
    }

    /// Send `key` as `prompt_cache_key` (one per gait session). Only the real
    /// OpenAI host receives it: other compatible hosts may reject unknown fields.
    pub fn with_prompt_cache_key(mut self, key: Option<String>) -> Self {
        self.prompt_cache_key = key;
        self
    }

    fn cache_key_for_host(&self) -> Option<String> {
        self.prompt_cache_key
            .clone()
            .filter(|k| !k.is_empty() && self.base_url.contains("api.openai.com"))
    }

    /// Send the gait session id as `x-opencode-session`. Only the `opencode`
    /// provider (set via the factory) uses this; empty ids are not stored.
    pub fn with_session_header(mut self, id: Option<String>) -> Self {
        self.session_header = id.filter(|v| !v.is_empty());
        self
    }

    fn session_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(id) = &self.session_header {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(id) {
                headers.insert(
                    reqwest::header::HeaderName::from_static("x-opencode-session"),
                    value,
                );
            }
        }
        headers
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
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
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
    /// Absent or `[]` on the usage-only chunk.
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<StreamUsage>,
    /// The model the server says it used.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
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
    /// A missing or null count is 0: one absent field must not drop the rest.
    #[serde(default, deserialize_with = "count_or_zero")]
    prompt_tokens: u32,
    #[serde(default, deserialize_with = "count_or_zero")]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
    /// Server-side timings some servers (oMLX) add to the usage object:
    /// time to first token in seconds, generation speed in tokens/second.
    #[serde(default)]
    time_to_first_token: Option<f64>,
    #[serde(default)]
    generation_tokens_per_second: Option<f64>,
}

fn count_or_zero<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    Ok(Option::<u32>::deserialize(d)?.unwrap_or(0))
}

#[derive(Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

impl StreamUsage {
    /// The normalized usage plus the reasoning tokens, which are a subset of
    /// the completion tokens (already inside `output_tokens`; never add them again).
    fn normalized(&self) -> (Usage, u32) {
        let cached = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .unwrap_or(0);
        let reasoning = self
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
            .unwrap_or(0);
        (
            crate::responses_wire::normalized_usage(
                self.prompt_tokens,
                self.completion_tokens,
                cached,
                0,
            ),
            reasoning,
        )
    }
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
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
            prompt_cache_key: self.cache_key_for_host(),
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
        headers.extend(self.session_headers());
        let request = crate::transport::SseRequest::new(url, headers, body_value);
        let mut call = crate::telemetry::ModelCall::begin(&crate::telemetry::RequestAttrs {
            temperature: params.temperature,
            top_p: params.top_p,
            max_tokens: params.max_tokens,
            model_id: self.model.clone(),
        });
        let stream =
            match crate::telemetry::within(call.span(), self.transport.stream(request)).await {
                Ok(stream) => stream,
                Err(error) => {
                    call.fail(&error);
                    return Err(error);
                }
            };
        Ok(Box::pin(body_stream(stream, call)))
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
    mut call: crate::telemetry::ModelCall,
) -> impl futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send + 'static {
    use futures_util::StreamExt;
    async_stream::try_stream! {
        tokio::pin!(guarded_events);
        let mut tool_ids:   HashMap<u32, String> = HashMap::new();
        let mut tool_names: HashMap<u32, String> = HashMap::new();
        let mut tool_args:  HashMap<u32, String> = HashMap::new();

        // A finish_reason chunk or [DONE] ends the answer, but OpenAI-compatible
        // servers send the usage chunk after it (and some after [DONE]), and the
        // turn loop stops reading at `Done`. So `Done` is held back until the
        // stream closes, waiting at most TRAILER_GRACE for a quiet server.
        let mut finished = false;
        let mut trailer_deadline = tokio::time::Instant::now();
        let mut tools_completed = false;
        // The last usage seen wins (a server may repeat it on the finish chunk
        // and again after it); it is yielded once, before Done.
        let mut last_usage: Option<Usage> = None;
        let mut received_bytes: usize = 0;

        loop {
            let next = if finished {
                tokio::time::timeout_at(trailer_deadline, guarded_events.next())
                    .await
                    .unwrap_or_default()
            } else {
                guarded_events.next().await
            };
            let Some(event) = next else { break };
            let event = match event {
                Ok(ev) => ev,
                // After the answer finished, a dropped connection loses nothing.
                Err(_) if finished => break,
                // Connection dropped mid-body before [DONE] → silent-truncation
                // case; surface StreamIncomplete. Parse/Utf8 errors stay Sse.
                Err(e) => {
                    call.fail(&e);
                    Err(e)?
                }
            };
            if event.data == "[DONE]" {
                // The protocol terminator: nothing follows it.
                call.answered();
                finished = true;
                break;
            }
            if event.data.is_empty() { continue; }

            let chunk: StreamChunkData = match serde_json::from_str(&event.data) {
                Ok(c) => c,
                Err(e) => {
                    log_unparsed_chunk(&event.data, &e);
                    continue;
                }
            };

            if let Some(model) = &chunk.model {
                call.response_model(model);
            }
            if let Some(usage) = &chunk.usage {
                // Reasoning tokens are a subset of output_tokens; they go on the
                // call span, not into Usage.
                let (usage, reasoning) = usage.normalized();
                call.usage(crate::telemetry::CallUsage {
                    input: usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens,
                    output: usage.output_tokens,
                    cached: usage.cache_read_tokens,
                    cache_write: usage.cache_write_tokens,
                    reasoning,
                });
                call.server_stats(
                    usage_server_ttft_ms(&chunk),
                    chunk.usage.as_ref().and_then(|u| u.generation_tokens_per_second),
                );
                last_usage = Some(usage);
            }

            for choice in &chunk.choices {
                if let Some(text) = &choice.delta.content {
                    if !text.is_empty() {
                        received_bytes += text.len();
                        call.first_token();
                        yield StreamChunk::Text(text.clone());
                    }
                }

                if let Some(thinking) = &choice.delta.reasoning_content {
                    if !thinking.is_empty() {
                        call.first_token();
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
                                call.first_token();
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

                if let Some(reason) = choice.finish_reason.as_deref() {
                    call.finish_reason(reason);
                }
                if matches!(choice.finish_reason.as_deref(), Some("tool_calls") | Some("stop")) {
                    for (idx, args_str) in tool_args.iter().filter(|_| !tools_completed) {
                        let id   = tool_ids.get(idx).cloned().unwrap_or_default();
                        let name = tool_names.get(idx).cloned().unwrap_or_default();
                        let args = crate::provider::parse_tool_args(args_str);
                        yield StreamChunk::ToolCallComplete { tool_call_id: id, tool_name: name, args };
                    }
                    call.answered();
                    tools_completed = true;
                    if !finished {
                        // Usage already seen: no trailer to wait for.
                        trailer_deadline = tokio::time::Instant::now()
                            + if last_usage.is_some() { std::time::Duration::ZERO } else { TRAILER_GRACE };
                    }
                    finished = true;
                }
            }
        }

        if let Some(usage) = last_usage {
            yield StreamChunk::StepUsage(usage);
        }
        if !finished {
            let error = ProviderError::StreamIncomplete { received_bytes };
            call.fail(&error);
            Err(error)?;
        }
        call.end();
        yield StreamChunk::Done;
    }
}

fn usage_server_ttft_ms(chunk: &StreamChunkData) -> Option<f64> {
    chunk
        .usage
        .as_ref()?
        .time_to_first_token
        .map(|seconds| seconds * 1000.0)
}

/// How long to keep reading after the answer finished, for a usage chunk.
const TRAILER_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// A chunk that does not parse is skipped, but never silently: a usage chunk
/// in an unexpected shape would otherwise vanish without a trace.
fn log_unparsed_chunk(data: &str, error: &serde_json::Error) {
    #[cfg(feature = "telemetry")]
    {
        let end = (0..=data.len().min(200))
            .rev()
            .find(|i| data.is_char_boundary(*i))
            .unwrap_or(0);
        tracing::warn!(reason = %format!("{error}: {}", &data[..end]), "unparsed chat stream chunk");
    }
    #[cfg(not(feature = "telemetry"))]
    let _ = (data, error);
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
            prompt_cache_key: None,
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
    fn reasoning_tokens_are_read_but_not_added_to_output() {
        let chunk: StreamChunkData = serde_json::from_value(serde_json::json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 30,
                "prompt_tokens_details": {"cached_tokens": 40},
                "completion_tokens_details": {"reasoning_tokens": 12}
            }
        }))
        .unwrap();
        let (usage, reasoning) = chunk.usage.unwrap().normalized();
        assert_eq!(reasoning, 12);
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens
            ),
            (60, 30, 40)
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
            prompt_cache_key: None,
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
                author_id: "agent-2".into(),
                trust: gait_types::types::Trust::Unverified,
            }],
        };
        let chat_msgs = to_chat_messages("", &[msg]);
        assert_eq!(chat_msgs.len(), 1);
        let content_str = chat_msgs[0].content.as_ref().unwrap().as_str().unwrap();
        assert_eq!(
            content_str,
            "<incoming-message from=\"agent-2\" trust=\"unverified\">Hello, world!</incoming-message>\n"
        );
    }

    #[test]
    fn prompt_cache_key_is_sent_only_to_the_real_openai_host() {
        let transport = || crate::transport::ProviderTransport::new(Default::default()).expect("t");
        let key = Some("session-abc".to_string());
        let openai = OpenAIProvider::new("k".into(), "m".into(), None, transport())
            .with_prompt_cache_key(key.clone());
        assert_eq!(openai.cache_key_for_host().as_deref(), Some("session-abc"));
        assert_eq!(openai.cache_key_for_host(), openai.cache_key_for_host());
        let other = OpenAIProvider::new(
            "k".into(),
            "m".into(),
            Some("https://api.groq.com/openai/v1".into()),
            transport(),
        )
        .with_prompt_cache_key(key);
        assert_eq!(other.cache_key_for_host(), None);
    }

    #[test]
    fn session_header_is_sent_for_opencode_only_and_is_stable() {
        let transport = || crate::transport::ProviderTransport::new(Default::default()).expect("t");
        let named = |id: &str| {
            OpenAIProvider::new("k".into(), "m".into(), None, transport())
                .with_session_header(Some(id.to_string()))
        };
        let s1 = named("s1");
        assert_eq!(
            s1.session_headers()
                .get("x-opencode-session")
                .and_then(|v| v.to_str().ok()),
            Some("s1")
        );
        assert_eq!(
            s1.session_headers().get("x-opencode-session"),
            s1.session_headers().get("x-opencode-session"),
            "the header must be stable across requests"
        );
        assert_ne!(
            s1.session_headers().get("x-opencode-session"),
            named("s2").session_headers().get("x-opencode-session"),
            "a different session id must yield a different header value"
        );
        assert!(
            OpenAIProvider::new("k".into(), "m".into(), None, transport())
                .session_headers()
                .is_empty(),
            "no session id must yield no header"
        );
        // Empty ids are never stored.
        assert!(
            OpenAIProvider::new("k".into(), "m".into(), None, transport())
                .with_session_header(Some(String::new()))
                .session_headers()
                .is_empty()
        );
    }

    /// Captures one request's header block over a local TCP stub, then answers
    /// with a minimal SSE stream so the provider call completes.
    async fn capture_request_head(listener: tokio::net::TcpListener) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed before the headers ended");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
        };
        let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
            sse.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        String::from_utf8_lossy(&buf[..head_end]).to_lowercase()
    }

    async fn wire_session_header(session: Option<String>) -> Option<String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(capture_request_head(listener));
        let provider = OpenAIProvider::new(
            "k".into(),
            "m".into(),
            Some(format!("http://{addr}/v1")),
            crate::transport::ProviderTransport::new(Default::default()).expect("t"),
        )
        .with_session_header(session);
        let msgs = vec![Message::User {
            id: "m1".into(),
            content: MessageContent::Text("hi".into()),
            attribution: Vec::new(),
        }];
        let _ = Provider::stream(&provider, "sys", &msgs, &[], &Default::default(), None).await;
        let head = tokio::time::timeout(std::time::Duration::from_secs(10), server)
            .await
            .expect("stub got a request")
            .unwrap();
        head.lines()
            .find_map(|l| l.strip_prefix("x-opencode-session:").map(str::trim))
            .map(String::from)
    }

    #[tokio::test]
    async fn session_header_reaches_the_wire_only_when_set() {
        assert_eq!(
            wire_session_header(Some("sess-1".into())).await.as_deref(),
            Some("sess-1")
        );
        assert_eq!(
            wire_session_header(Some("sess-1".into())).await.as_deref(),
            Some("sess-1"),
            "same session id must produce the same header on every request"
        );
        assert_ne!(
            wire_session_header(Some("sess-1".into())).await.as_deref(),
            wire_session_header(Some("sess-2".into())).await.as_deref(),
            "a second session id must differ"
        );
        assert_eq!(
            wire_session_header(None).await,
            None,
            "the header must be absent without a session id"
        );
    }

    /// Collects the fields of every event, as text.
    #[cfg(feature = "telemetry")]
    #[derive(Clone, Default)]
    struct Events(std::sync::Arc<std::sync::Mutex<String>>);

    #[cfg(feature = "telemetry")]
    impl tracing::Subscriber for Events {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Text<'a>(&'a mut String);
            impl tracing::field::Visit for Text<'_> {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push_str(&format!("{}={value:?};", field.name()));
                }
            }
            event.record(&mut Text(&mut self.0.lock().unwrap()));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn an_unparsed_chunk_is_logged_without_its_text() {
        let data = r#"{"choices":"hunter2-reply"}"#;
        let error = serde_json::from_str::<StreamChunkData>(data).err().unwrap();
        let events = Events::default();
        tracing::subscriber::with_default(events.clone(), || log_unparsed_chunk(data, &error));
        let seen = events.0.lock().unwrap().clone();
        assert!(seen.contains("unparsed chat stream chunk"), "{seen}");
        assert!(!seen.contains("hunter2-reply"), "{seen}");
    }
}
