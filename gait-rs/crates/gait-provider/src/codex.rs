//! ChatGPT/Codex subscription transport.
//!
//! The request route and subscription headers are adapted from OpenAI Codex,
//! Apache-2.0, commit 315195492c80fdade38e917c18f9584efd599304:
//! `codex-rs/model-provider-info/src/lib.rs` and
//! `codex-rs/model-provider/src/bearer_auth_provider.rs`. See
//! `THIRD_PARTY_NOTICES.md` for the complete attribution.

use crate::provider::{ChunkStream, Provider, ProviderError, RequestKind};
use gait_auth::codex::{credential_details, CODEX_PROVIDER_ID};
use gait_auth::{ResolvedCredential, SubscriptionCredential};
use gait_types::types::{Message, MessageContent, Tool};
use serde_json::Value;
use std::sync::Arc;

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
    transport: crate::transport::ProviderTransport,
}

impl CodexResponsesProvider {
    pub fn new(
        credential: SubscriptionCredential,
        model: String,
        base_url: Option<String>,
        transport: crate::transport::ProviderTransport,
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
            transport,
        })
    }

    fn request_body(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
        params: &gait_types::config::LlmParamsConfig,
        kind: RequestKind,
    ) -> Value {
        crate::responses_wire::request_body(&self.model, system, messages, tools, params, kind)
    }
}

#[async_trait::async_trait]
impl Provider for CodexResponsesProvider {
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
        params: &gait_types::config::LlmParamsConfig,
        tail_context: Option<&str>,
    ) -> Result<ChunkStream, ProviderError> {
        self.stream_with_kind(
            system,
            messages,
            tools,
            params,
            tail_context,
            RequestKind::Turn,
        )
        .await
    }

    async fn stream_with_kind(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
        params: &gait_types::config::LlmParamsConfig,
        tail_context: Option<&str>,
        kind: RequestKind,
    ) -> Result<ChunkStream, ProviderError> {
        let mut effective_messages = messages.to_vec();
        if let Some(tail) = tail_context {
            effective_messages.push(Message::User {
                content: MessageContent::Text(tail.to_string()),
                id: String::new(),
                attribution: Vec::new(),
            });
        }
        let body = self.request_body(system, &effective_messages, tools, params, kind);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            crate::transport::sensitive_header(&format!("Bearer {}", self.access_token))?,
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        if let Some(account_id) = &self.account_id {
            headers.insert(
                reqwest::header::HeaderName::from_static("chatgpt-account-id"),
                crate::transport::sensitive_header(account_id)?,
            );
        }
        if self.fedramp {
            headers.insert(
                reqwest::header::HeaderName::from_static("x-openai-fedramp"),
                reqwest::header::HeaderValue::from_static("true"),
            );
        }
        let request = crate::transport::SseRequest::new(
            format!("{}/responses", self.base_url.trim_end_matches('/')),
            headers,
            body,
        );
        Ok(Box::pin(crate::responses_wire::response_stream(
            self.transport.stream(request).await?,
            codex_failure_format,
        )))
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

fn codex_failure_format(failure: &crate::responses_wire::ResponsesFailure) -> String {
    match (failure.code.as_deref(), failure.message.as_deref()) {
        (Some(code), Some(message)) => {
            format!("Codex Responses request failed ({code}): {message}")
        }
        (Some(code), None) => format!("Codex Responses request failed ({code})"),
        (None, Some(message)) => format!("Codex Responses request failed: {message}"),
        (None, None) => "Codex Responses request failed".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::StreamChunk;
    use gait_types::types::{
        AssistantContent, AssistantPart, ToolCall, ToolDef, ToolResult, ToolResultKind,
    };
    use serde_json::json;

    fn test_credential(access_token: &str) -> SubscriptionCredential {
        SubscriptionCredential {
            provider_id: CODEX_PROVIDER_ID.into(),
            method_id: "codex-subscription".into(),
            version: 1,
            expires_at_unix_s: None,
            account_label: None,
            secret: json!({
                "id_token": "e30.e30.sig",
                "access_token": access_token,
                "refresh_token": "refresh-test",
                "account_id": "acct-test"
            }),
        }
    }

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
            crate::transport::ProviderTransport::new(Default::default()).expect("transport"),
        );
        assert!(matches!(result, Err(error) if error.contains("different provider")));
    }

    #[test]
    fn reads_cached_input_tokens_from_response_usage() {
        let usage = json!({
            "input_tokens": 1200,
            "output_tokens": 30,
            "input_tokens_details": {"cached_tokens": 900}
        });
        assert_eq!(crate::responses_wire::cached_input_tokens(&usage), 900);
    }

    #[test]
    fn compaction_omits_unsupported_output_token_limit() {
        let provider = CodexResponsesProvider::new(
            test_credential("access-test"),
            "gpt-5.3-codex-spark".into(),
            None,
            crate::transport::ProviderTransport::new(Default::default()).expect("transport"),
        )
        .expect("provider");
        let params = gait_types::config::LlmParamsConfig {
            max_tokens: Some(7936),
            ..Default::default()
        };

        let turn = provider.request_body("", &[], &[], &params, RequestKind::Turn);
        assert_eq!(turn.get("max_output_tokens"), Some(&json!(7936)));

        let compaction = provider.request_body("", &[], &[], &params, RequestKind::Compaction);
        assert!(compaction.get("max_output_tokens").is_none());
    }

    #[test]
    fn response_failure_preserves_provider_code_and_message() {
        let event = json!({
            "response": {
                "status": "failed",
                "error": {
                    "code": "server_error",
                    "message": "model backend unavailable"
                }
            }
        });
        assert_eq!(
            codex_failure_format(&crate::responses_wire::failure_from_value(&event, false)),
            "Codex Responses request failed (server_error): model backend unavailable"
        );
    }

    #[tokio::test]
    async fn retries_retryable_status_before_first_event() {
        use futures_util::StreamExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        async fn read_request(socket: &mut tokio::net::TcpStream) {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut expected_len = None;
            loop {
                let read = socket.read(&mut chunk).await.expect("read request");
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
                if expected_len.is_none() {
                    if let Some(header_end) =
                        bytes.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&bytes[..header_end]);
                        let content_len = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        expected_len = Some(header_end + 4 + content_len);
                    }
                }
                if expected_len.is_some_and(|expected| bytes.len() >= expected) {
                    break;
                }
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                observed.fetch_add(1, Ordering::SeqCst);
                read_request(&mut socket).await;
                if attempt == 0 {
                    socket
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 5\r\nConnection: close\r\n\r\nretry",
                        )
                        .await
                        .expect("write 503");
                } else {
                    let body = "event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    socket
                        .write_all(headers.as_bytes())
                        .await
                        .expect("write headers");
                    socket.write_all(body.as_bytes()).await.expect("write SSE");
                }
            }
        });

        let provider = CodexResponsesProvider::new(
            test_credential("access-test"),
            "gpt-test".into(),
            Some(format!("http://{addr}")),
            crate::transport::ProviderTransport::new(crate::transport::TransportConfig {
                initial_backoff: std::time::Duration::ZERO,
                max_backoff: std::time::Duration::ZERO,
                ..Default::default()
            })
            .expect("transport"),
        )
        .expect("provider");
        let mut stream = provider
            .stream("", &[], &[], &Default::default(), None)
            .await
            .expect("503 should be retried before the first event");

        let mut saw_done = false;
        while let Some(item) = stream.next().await {
            if matches!(item.expect("stream item"), StreamChunk::Done) {
                saw_done = true;
            }
        }
        assert!(saw_done, "successful retry should deliver Done");
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn wraps_user_message_in_room_message_tag_when_attributed() {
        let provider = CodexResponsesProvider::new(
            test_credential("access-test"),
            "gpt-5.3-codex".into(),
            None,
            crate::transport::ProviderTransport::new(Default::default()).expect("transport"),
        )
        .expect("provider");

        let message = Message::User {
            content: MessageContent::Text("hello world".to_string()),
            id: "msg-123".to_string(),
            attribution: vec![gait_types::types::Attribution {
                author_id: "agent-2".into(),
                context: "room-general".into(),
                trust: gait_types::types::Trust::Unverified,
            }],
        };

        let body =
            provider.request_body("", &[message], &[], &Default::default(), RequestKind::Turn);
        let input_content = body
            .get("input")
            .and_then(|input| input.as_array())
            .and_then(|arr| arr.first())
            .and_then(|msg| msg.get("content"))
            .and_then(|content| content.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("text"))
            .and_then(|text| text.as_str())
            .expect("text field in message");

        assert_eq!(
            input_content,
            "<incoming-message from=\"agent-2\" context=\"room-general\" trust=\"unverified\">hello world</incoming-message>\n"
        );
    }

    #[test]
    fn request_body_matches_codex_golden() {
        let provider = CodexResponsesProvider::new(
            test_credential("access-test"),
            "gpt-5.3-codex".into(),
            None,
            crate::transport::ProviderTransport::new(Default::default()).expect("transport"),
        )
        .expect("provider");
        let messages = vec![
            Message::User {
                content: MessageContent::Text("user question".into()),
                id: "user-1".into(),
                attribution: Vec::new(),
            },
            Message::Assistant {
                content: AssistantContent::Text("assistant answer".into()),
                id: "assistant-1".into(),
            },
        ];
        let params = gait_types::config::LlmParamsConfig {
            max_tokens: Some(123),
            ..Default::default()
        };
        let body =
            provider.request_body("system prompt", &messages, &[], &params, RequestKind::Turn);
        assert_eq!(
            body,
            json!({
                "model": "gpt-5.3-codex",
                "input": [
                    {"role": "user", "content": [{"type": "input_text", "text": "user question"}]},
                    {"role": "assistant", "content": [{"type": "output_text", "text": "assistant answer"}]}
                ],
                "stream": true,
                "store": false,
                "parallel_tool_calls": false,
                "instructions": "system prompt",
                "max_output_tokens": 123
            })
        );
    }

    #[test]
    fn request_body_golden_with_tools_and_tool_results() {
        let provider = CodexResponsesProvider::new(
            test_credential("access-test"),
            "gpt-5.3-codex".into(),
            None,
            crate::transport::ProviderTransport::new(Default::default()).expect("transport"),
        )
        .expect("provider");
        let tool = ToolDef {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            execute: Box::new(|_| Box::pin(async { Ok(json!(null)) })),
            permission: None,
        };
        let messages = vec![
            Message::Assistant {
                content: AssistantContent::Parts(vec![AssistantPart::ToolCall(ToolCall {
                    tool_call_id: "call-1".into(),
                    tool_name: "read_file".into(),
                    args: json!({"path": "notes.txt"}),
                })]),
                id: "assistant-1".into(),
            },
            Message::ToolResult {
                content: vec![ToolResult {
                    kind: ToolResultKind::ToolResult,
                    tool_call_id: "call-1".into(),
                    result: json!("contents"),
                }],
                id: "tool-1".into(),
            },
        ];
        let params = gait_types::config::LlmParamsConfig {
            max_tokens: Some(321),
            ..Default::default()
        };
        let body = provider.request_body(
            "",
            &messages,
            &[tool.arced()],
            &params,
            RequestKind::Compaction,
        );
        assert_eq!(
            body,
            json!({
                "model": "gpt-5.3-codex",
                "input": [
                    {"type": "function_call", "call_id": "call-1", "name": "read_file", "arguments": "{\"path\":\"notes.txt\"}"},
                    {"type": "function_call_output", "call_id": "call-1", "output": "\"contents\""}
                ],
                "stream": true,
                "store": false,
                "parallel_tool_calls": false,
                "tools": [{
                    "type": "function",
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"]
                    }
                }]
            })
        );
    }
}
