//! ChatGPT/Codex subscription transport.
//!
//! The request route and subscription headers are adapted from OpenAI Codex,
//! Apache-2.0, commit 315195492c80fdade38e917c18f9584efd599304:
//! `codex-rs/model-provider-info/src/lib.rs` and
//! `codex-rs/model-provider/src/bearer_auth_provider.rs`. See
//! `THIRD_PARTY_NOTICES.md` for the complete attribution.

use crate::provider::{ChunkStream, Provider, ProviderError, StreamChunk};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use gait_auth::codex::{credential_details, CODEX_PROVIDER_ID};
use gait_auth::{ResolvedCredential, SubscriptionCredential};
use gait_types::types::{AssistantContent, AssistantPart, Message, MessageContent, Tool, Usage};
use serde_json::{json, Value};
use std::collections::HashMap;

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// A provider for the private Codex Responses API. It is deliberately separate
/// from the public OpenAI Chat Completions adapter: the endpoints, request
/// schema, and subscription headers are not interchangeable.
pub struct CodexResponsesProvider {
    access_token: String,
    account_id: Option<String>,
    fedramp: bool,
    model: String,
    base_url: String,
    http: reqwest::Client,
}

impl CodexResponsesProvider {
    pub fn new(
        credential: SubscriptionCredential,
        model: String,
        base_url: Option<String>,
    ) -> Result<Self, String> {
        if credential.provider_id != CODEX_PROVIDER_ID {
            return Err("subscription credential belongs to a different provider".into());
        }
        let details = credential_details(&ResolvedCredential::Subscription(credential.clone()))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            access_token: details.access_token,
            account_id: details.account_id,
            fedramp: details.is_fedramp_account,
            model,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            http: reqwest::Client::new(),
        })
    }

    fn request_body(&self, system: &str, messages: &[Message], tools: &[Box<dyn Tool>]) -> Value {
        let mut input = Vec::new();
        for message in messages {
            match message {
                Message::User { content, .. } => input.push(json!({
                    "role": "user", "content": [{"type": "input_text", "text": text_content(content)}]
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
            "model": self.model, "input": input, "stream": true,
            "store": false, "parallel_tool_calls": false,
        });
        if !system.is_empty() {
            body["instructions"] = Value::String(system.to_string());
        }
        if !functions.is_empty() {
            body["tools"] = Value::Array(functions);
        }
        body
    }
}

fn text_content(content: &MessageContent) -> String {
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

#[async_trait::async_trait]
impl Provider for CodexResponsesProvider {
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Box<dyn Tool>],
        _params: &gait_types::config::LlmParamsConfig,
        tail_context: Option<&str>,
    ) -> Result<ChunkStream, ProviderError> {
        let mut effective_messages = messages.to_vec();
        if let Some(tail) = tail_context {
            effective_messages.push(Message::User {
                content: MessageContent::Text(tail.to_string()),
                id: String::new(),
            });
        }
        let body = self.request_body(system, &effective_messages, tools);
        let mut request = self
            .http
            .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
            .bearer_auth(&self.access_token)
            .header("Content-Type", "application/json")
            .json(&body);
        if let Some(account_id) = &self.account_id {
            request = request.header("ChatGPT-Account-ID", account_id);
        }
        if self.fedramp {
            request = request.header("X-OpenAI-FedRAMP", "true");
        }
        let response = request.send().await.map_err(ProviderError::from_reqwest)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status,
                body,
                retry_after: None,
            });
        }
        Ok(Box::pin(response_stream(
            response.bytes_stream().eventsource(),
        )))
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

fn response_stream(
    events: impl futures_util::Stream<
            Item = Result<
                eventsource_stream::Event,
                eventsource_stream::EventStreamError<reqwest::Error>,
            >,
        > + Send
        + 'static,
) -> impl futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send + 'static {
    async_stream::try_stream! {
        tokio::pin!(events);
        let mut calls: HashMap<String, (String, String)> = HashMap::new();
        let mut terminal = false;
        let mut received_bytes = 0usize;
        while let Some(event) = events.next().await {
            let event = event.map_err(|error| ProviderError::Sse(error.to_string()))?;
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
                        yield StreamChunk::StepUsage(Usage { input_tokens: usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32, output_tokens: usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32, cache_read_tokens: 0, cache_write_tokens: 0 });
                    }
                    yield StreamChunk::Done;
                    terminal = true;
                    break;
                }
                "response.failed" => Err(ProviderError::Other("Codex Responses request failed".into()))?,
                _ => {}
            }
        }
        if !terminal { Err(ProviderError::StreamIncomplete { received_bytes })?; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_codex_subscription() {
        let result = CodexResponsesProvider::new(
            SubscriptionCredential {
                provider_id: "other".into(),
                method_id: "m".into(),
                version: 1,
                expires_at_unix_s: None,
                account_label: None,
                secret: json!({}),
            },
            "gpt-5".into(),
            None,
        );
        assert!(matches!(result, Err(error) if error.contains("different provider")));
    }
}
