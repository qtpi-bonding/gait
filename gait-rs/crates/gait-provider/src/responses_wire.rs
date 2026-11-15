use crate::provider::{ProviderError, RequestKind, StreamChunk};
use futures_util::StreamExt;
use gait_types::config::LlmParamsConfig;
use gait_types::types::{AssistantContent, AssistantPart, Message, MessageContent, Tool, Usage};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub fn request_body(
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[Arc<dyn Tool>],
    params: &LlmParamsConfig,
    kind: RequestKind,
    prompt_cache_key: Option<&str>,
) -> Value {
    let mut input = Vec::new();
    for message in messages {
        match message {
            Message::User { content, attribution, .. } => input.push(json!({
                "role": "user", "content": [{"type": "input_text", "text": gait_types::types::render_user_text(attribution, &text_content(content))}]
            })),
            Message::Assistant { content, .. } => {
                let mut content_items = Vec::new();
                let mut calls = Vec::new();
                match content {
                    AssistantContent::Text(text) => content_items.push(json!({"type":"output_text", "text":text})),
                    AssistantContent::Parts(parts) => for part in parts {
                        match part {
                            AssistantPart::Text { text } => content_items.push(json!({"type":"output_text", "text":text})),
                            AssistantPart::Thinking { content, .. } => content_items.push(json!({"type":"output_text", "text":content})),
                            AssistantPart::ToolCall(call) => calls.push(json!({
                                "type": "function_call", "call_id": call.tool_call_id,
                                "name": call.tool_name,
                                "arguments": crate::provider::coerce_tool_input(call.args.clone()).to_string(),
                            })),
                        }
                    },
                }
                if !content_items.is_empty() {
                    input.push(json!({"role":"assistant", "content":content_items}));
                }
                input.extend(calls);
            }
            Message::ToolResult { content, .. } => input.extend(content.iter().map(|result| json!({
                "type": "function_call_output", "call_id": result.tool_call_id,
                "output": result.result.to_string(),
            }))),
        }
    }
    let functions: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function", "name": tool.name(),
                "description": gait_types::types::render_description(tool.as_ref()),
                "parameters": tool.parameters(),
            })
        })
        .collect();
    let mut body = json!({
        "model": model, "input": input, "stream": true,
        "store": false, "parallel_tool_calls": false,
    });
    // One stable key per gait session routes every call of the session to the
    // same prompt cache; without it prefix hits depend on request routing.
    if let Some(key) = prompt_cache_key.filter(|k| !k.is_empty()) {
        body["prompt_cache_key"] = Value::String(key.to_string());
    }
    if !system.is_empty() {
        body["instructions"] = Value::String(system.to_string());
    }
    if !functions.is_empty() {
        body["tools"] = Value::Array(functions);
    }
    if kind == RequestKind::Turn {
        if let Some(max_tokens) = params.max_tokens {
            body["max_output_tokens"] = Value::from(max_tokens);
        }
    }
    body
}

pub struct ResponsesFailure {
    pub code: Option<String>,
    // Compaction requests intentionally omit max_output_tokens: Responses uses
    // that field as a completion cap, while compaction needs the full budget.
    pub param: Option<String>,
    pub message: Option<String>,
}

pub fn response_stream(
    events: impl futures_util::Stream<Item = Result<eventsource_stream::Event, ProviderError>>
        + Send
        + 'static,
    fmt: fn(&ResponsesFailure) -> String,
) -> impl futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send + 'static {
    async_stream::try_stream! {
        tokio::pin!(events);
        let mut calls: HashMap<String, (String, String)> = HashMap::new();
        let mut terminal = false;
        let mut received_bytes = 0usize;
        while let Some(event) = events.next().await {
            let event = event?;
            let value: Value = match serde_json::from_str(&event.data) { Ok(value) => value, Err(_) => continue };
            match event.event.as_str() {
                "response.output_text.delta" => if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    received_bytes += delta.len();
                    yield StreamChunk::Text(delta.to_string());
                },
                "response.output_item.added" => {
                    let item = value.get("item").unwrap_or(&Value::Null);
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        if let (Some(call_id), Some(name)) = (item.get("call_id").and_then(Value::as_str), item.get("name").and_then(Value::as_str)) {
                            calls.insert(call_id.to_string(), (name.to_string(), item.get("arguments").and_then(Value::as_str).unwrap_or("").to_string()));
                        }
                    }
                }
                "response.function_call_arguments.delta" => if let (Some(call_id), Some(delta)) = (value.get("call_id").and_then(Value::as_str), value.get("delta").and_then(Value::as_str)) {
                    let entry = calls.entry(call_id.to_string()).or_insert_with(|| (value.get("name").and_then(Value::as_str).unwrap_or("").to_string(), String::new()));
                    entry.1.push_str(delta);
                    yield StreamChunk::ToolCallDelta { tool_call_id: call_id.to_string(), tool_name: entry.0.clone(), args_delta: delta.to_string() };
                },
                "response.output_item.done" => {
                    let item = value.get("item").unwrap_or(&Value::Null);
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        if let (Some(call_id), Some(name)) = (item.get("call_id").and_then(Value::as_str), item.get("name").and_then(Value::as_str)) {
                            let args = item.get("arguments").and_then(Value::as_str).or_else(|| calls.get(call_id).map(|(_, args)| args.as_str())).unwrap_or("");
                            yield StreamChunk::ToolCallComplete { tool_call_id: call_id.to_string(), tool_name: name.to_string(), args: crate::provider::parse_tool_args(args) };
                        }
                    }
                }
                "response.completed" => {
                    if let Some(usage) = value.get("response").and_then(|r| r.get("usage")) {
                        yield StreamChunk::StepUsage(normalized_usage(
                            usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
                            usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
                            cached_input_tokens(usage),
                            cache_write_input_tokens(usage),
                        ));
                    }
                    yield StreamChunk::Done;
                    terminal = true;
                    break;
                }
                "response.failed" => {
                    let failure = failure_from_value(&value, false);
                    Err(ProviderError::Other(fmt(&failure)))?;
                }
                "error" => {
                    let failure = failure_from_value(&value, true);
                    Err(ProviderError::Other(fmt(&failure)))?;
                }
                "response.incomplete" => {
                    let reason = value.get("response")
                        .and_then(|response| response.get("incomplete_details"))
                        .and_then(|details| details.get("reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    Err(ProviderError::Other(format!("Responses request incomplete: {reason}")))?;
                }
                _ => {}
            }
        }
        if !terminal { Err(ProviderError::StreamIncomplete { received_bytes })?; }
    }
}

pub(crate) fn failure_from_value(value: &Value, top_level_error: bool) -> ResponsesFailure {
    let error = if top_level_error {
        value.get("error").unwrap_or(value)
    } else {
        value
            .get("response")
            .and_then(|response| response.get("error"))
            .or_else(|| value.get("error"))
            .unwrap_or(&Value::Null)
    };
    ResponsesFailure {
        code: error.get("code").and_then(Value::as_str).map(str::to_owned),
        param: error
            .get("param")
            .and_then(Value::as_str)
            .map(str::to_owned),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// OpenAI-style providers report `input_tokens` INCLUDING the cached and the
/// cache-written tokens. Gait's `Usage.input_tokens` means uncached input (the
/// Anthropic convention that context estimates, stats totals and cost all
/// assume), so subtract both here, at the provider boundary.
///
/// Assumption: `cache_write_tokens` is a subset of `input_tokens`, like
/// `cached_tokens` (they are siblings in `input_tokens_details`). Every sample
/// seen so far had it at 0, so this is unverified; if writes turn out not to
/// be included, `input` is low by the write amount on turns that write cache.
pub(crate) fn normalized_usage(input: u32, output: u32, cached: u32, written: u32) -> Usage {
    Usage {
        input_tokens: input.saturating_sub(cached).saturating_sub(written),
        output_tokens: output,
        cache_read_tokens: cached,
        cache_write_tokens: written,
    }
}

pub(crate) fn cache_write_input_tokens(usage: &Value) -> u32 {
    usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
}

pub(crate) fn cached_input_tokens(usage: &Value) -> u32 {
    usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
}

pub fn text_content(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|part| match part {
                gait_types::types::ContentPart::Text { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{stream, StreamExt};

    fn event(name: &str, data: Value) -> eventsource_stream::Event {
        eventsource_stream::Event {
            event: name.to_string(),
            data: data.to_string(),
            id: String::new(),
            retry: None,
        }
    }

    #[test]
    fn usage_input_excludes_cached_and_cache_written_tokens() {
        let usage = json!({
            "input_tokens": 3000,
            "input_tokens_details": {"cached_tokens": 2688, "cache_write_tokens": 100}
        });
        let normalized = normalized_usage(
            3000,
            40,
            cached_input_tokens(&usage),
            cache_write_input_tokens(&usage),
        );
        assert_eq!(
            (normalized.input_tokens, normalized.output_tokens, normalized.cache_read_tokens, normalized.cache_write_tokens),
            (212, 40, 2688, 100)
        );
        // Never underflows when a backend reports more cache than input.
        assert_eq!(normalized_usage(10, 0, 50, 5).input_tokens, 0);
    }

    #[tokio::test]
    async fn completed_event_reports_uncached_input_and_cache_write() {
        let events = stream::iter([Ok(event(
            "response.completed",
            json!({"response": {"usage": {
                "input_tokens": 3000, "output_tokens": 40,
                "input_tokens_details": {"cached_tokens": 2688, "cache_write_tokens": 100}
            }}}),
        ))]);
        let chunks: Vec<_> = response_stream(events, |_| String::new()).collect().await;
        let usage = chunks
            .into_iter()
            .find_map(|chunk| match chunk {
                Ok(StreamChunk::StepUsage(usage)) => Some(usage),
                _ => None,
            })
            .expect("usage chunk");
        assert_eq!(
            (usage.input_tokens, usage.cache_read_tokens, usage.cache_write_tokens),
            (212, 2688, 100)
        );
    }

    #[tokio::test]
    async fn error_event_mid_stream_carries_code_and_param_to_formatter() {
        let events = stream::iter([Ok(event(
            "error",
            json!({"error": {"code": "bad_request", "param": "model", "message": "invalid model"}}),
        ))]);
        let output = response_stream(events, |failure| {
            format!(
                "code={} param={} message={}",
                failure.code.as_deref().unwrap_or("missing"),
                failure.param.as_deref().unwrap_or("missing"),
                failure.message.as_deref().unwrap_or("missing")
            )
        });
        tokio::pin!(output);
        let result = output.next().await.expect("error event result");
        assert!(
            matches!(result, Err(ProviderError::Other(message)) if message == "code=bad_request param=model message=invalid model")
        );
    }

    #[tokio::test]
    async fn error_event_top_level_shape_carries_code_param_message() {
        let events = stream::iter([Ok(event(
            "error",
            json!({"type": "error", "code": "bad_request", "param": "model", "message": "invalid model"}),
        ))]);
        let output = response_stream(events, |failure| {
            format!(
                "Codex Responses request failed ({}): {} (param={})",
                failure.code.as_deref().unwrap_or("missing"),
                failure.message.as_deref().unwrap_or("missing"),
                failure.param.as_deref().unwrap_or("missing")
            )
        });
        tokio::pin!(output);
        let result = output.next().await.expect("error event result");
        assert!(
            matches!(result, Err(ProviderError::Other(message)) if message == "Codex Responses request failed (bad_request): invalid model (param=model)")
        );
    }

    #[tokio::test]
    async fn incomplete_event_is_terminal_error_with_reason() {
        let events = stream::iter([Ok(event(
            "response.incomplete",
            json!({"response": {"incomplete_details": {"reason": "max_output_tokens"}}}),
        ))]);
        let output = response_stream(events, |_| "unexpected formatter".to_string());
        tokio::pin!(output);
        let result = output.next().await.expect("incomplete event result");
        assert!(
            matches!(result, Err(ProviderError::Other(message)) if message.contains("max_output_tokens"))
        );
    }

    #[tokio::test]
    async fn incomplete_event_without_reason_has_sane_text() {
        let events = stream::iter([Ok(event("response.incomplete", json!({"response": {}})))]);
        let output = response_stream(events, |_| "unexpected".to_string());
        tokio::pin!(output);
        let result = output.next().await.expect("incomplete event result");
        assert!(
            matches!(result, Err(ProviderError::Other(message)) if message == "Responses request incomplete: unknown")
        );
    }

    #[tokio::test]
    async fn failed_event_unchanged_for_codex_format() {
        let events = stream::iter([Ok(event(
            "response.failed",
            json!({"response": {"error": {"code": "server_error", "message": "model backend unavailable"}}}),
        ))]);
        let output = response_stream(events, |failure| {
            match (failure.code.as_deref(), failure.message.as_deref()) {
                (Some(code), Some(message)) => {
                    format!("Codex Responses request failed ({code}): {message}")
                }
                (Some(code), None) => format!("Codex Responses request failed ({code})"),
                (None, Some(message)) => format!("Codex Responses request failed: {message}"),
                (None, None) => "Codex Responses request failed".to_string(),
            }
        });
        tokio::pin!(output);
        let result = output.next().await.expect("failed event result");
        assert!(
            matches!(result, Err(ProviderError::Other(message)) if message == "Codex Responses request failed (server_error): model backend unavailable")
        );
    }
}

#[cfg(test)]
mod cache_prefix_tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::User {
            content: MessageContent::Text(text.into()),
            id: String::new(),
            attribution: Vec::new(),
        }
    }

    #[test]
    fn a_later_request_extends_the_earlier_one_byte_for_byte() {
        let params = LlmParamsConfig::default();
        let history = vec![user("first"), user("second")];
        let earlier = request_body("m", "sys", &history[..1], &[], &params, RequestKind::Turn, Some("k"));
        let later = request_body("m", "sys", &history, &[], &params, RequestKind::Turn, Some("k"));
        let (a, b) = (earlier["input"].as_array().unwrap(), later["input"].as_array().unwrap());
        assert!(b.len() > a.len());
        assert_eq!(a[..], b[..a.len()], "history prefix must not change between calls");
        for field in ["model", "instructions", "tools", "prompt_cache_key"] {
            assert_eq!(earlier.get(field), later.get(field), "{field} must be stable");
        }
    }
}
