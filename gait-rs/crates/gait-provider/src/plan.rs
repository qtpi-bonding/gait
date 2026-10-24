use crate::provider::{ChunkStream, Provider, ProviderError, RequestKind};
use gait_auth::signin::{BrainSecret, SIGNIN_PROVIDER_ID};
use gait_auth::SubscriptionCredential;
use gait_types::types::{Message, Tool};
use serde_json::Value;
use std::sync::Arc;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct PlanResponsesProvider {
    access_token: String,
    model: String,
    base_url: String,
    transport: crate::transport::ProviderTransport,
}

impl std::fmt::Debug for PlanResponsesProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanResponsesProvider")
            .field("access_token", &"<redacted>")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("transport", &"<configured>")
            .finish()
    }
}

impl PlanResponsesProvider {
    pub fn new(
        credential: SubscriptionCredential,
        model: String,
        base_url: Option<String>,
        transport: crate::transport::ProviderTransport,
    ) -> Result<Self, String> {
        if credential.provider_id != SIGNIN_PROVIDER_ID {
            return Err("subscription credential belongs to a different provider".into());
        }
        let secret: BrainSecret = serde_json::from_value(credential.secret)
            .map_err(|_| "invalid credential: expected BrainSecret".to_string())?;
        let tokens = secret.tokens.ok_or_else(|| {
            "invalid credential: tokens are missing; run `gait auth login openai-signin`"
                .to_string()
        })?;
        if tokens.access_token.is_empty() {
            return Err(
                "invalid credential: access token is missing; run `gait auth login openai-signin`"
                    .into(),
            );
        }
        Ok(Self {
            access_token: tokens.access_token,
            model,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into()),
            transport,
        })
    }

    fn body(
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
impl Provider for PlanResponsesProvider {
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
        let mut effective = messages.to_vec();
        if let Some(tail) = tail_context {
            effective.push(Message::User {
                content: gait_types::types::MessageContent::Text(tail.into()),
                id: String::new(),
                attribution: Vec::new(),
            });
        }
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            crate::transport::sensitive_header(&format!("Bearer {}", self.access_token))?,
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        let request = crate::transport::SseRequest::new(
            format!("{}/responses", self.base_url.trim_end_matches('/')),
            headers,
            self.body(system, &effective, tools, params, kind),
        );
        let events = self
            .transport
            .stream(request)
            .await
            .map_err(|error| match error {
                ProviderError::Http {
                    status,
                    body,
                    retry_after,
                    request_id,
                } => ProviderError::Http {
                    status,
                    body: map_http_error(status, &body, request_id.as_deref()),
                    retry_after,
                    request_id,
                },
                error => error,
            })?;
        Ok(Box::pin(crate::responses_wire::response_stream(
            events,
            plan_failure_format,
        )))
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

pub fn map_plan_error(
    status: Option<u16>,
    code: Option<&str>,
    param: Option<&str>,
    detail: Option<&str>,
    request_id: Option<&str>,
) -> String {
    match code {
        Some("subscription_sharing_user_not_eligible") => {
            "not available for this account/workspace".into()
        }
        Some("subscription_sharing_usage_limit_exceeded") => {
            "plan limit reached; see ChatGPT Settings > Usage".into()
        }
        Some("subscription_sharing_usage_unavailable")
        | Some("subscription_sharing_user_unavailable") => {
            "plan service temporarily unavailable; retry".into()
        }
        Some("subscription_sharing_unsupported_capability") => {
            format!("unsupported capability: {}", param.unwrap_or("unknown"))
        }
        Some("subscription_sharing_route_not_supported") => {
            "configuration error: route not supported".into()
        }
        Some(code) if code.starts_with("chatpass_v2_") => {
            "configuration error: ChatGPT plan configuration is not supported".into()
        }
        Some("subscription_sharing_invalid_user") => format_request_id(
            "invalid subscription user; the next turn refreshes the credential",
            request_id,
        ),
        _ if status == Some(401) => {
            "credential expired mid-turn; the next turn refreshes it".into()
        }
        _ if status == Some(503) => "plan service temporarily unavailable; retry".into(),
        _ => {
            let text = detail.or(code).unwrap_or("request failed");
            format_request_id(text, request_id)
        }
    }
}

fn format_request_id(message: &str, request_id: Option<&str>) -> String {
    match request_id {
        Some(id) => format!("{message} (request id: {id})"),
        None => message.into(),
    }
}

fn map_http_error(status: u16, body: &str, request_id: Option<&str>) -> String {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = value.get("error").unwrap_or(&value);
    let code = error.get("code").and_then(Value::as_str);
    let param = error.get("param").and_then(Value::as_str);
    let detail = value
        .get("detail")
        .and_then(Value::as_str)
        .or_else(|| error.get("message").and_then(Value::as_str));
    map_plan_error(Some(status), code, param, detail, request_id)
}

fn plan_failure_format(failure: &crate::responses_wire::ResponsesFailure) -> String {
    map_plan_error(
        None,
        failure.code.as_deref(),
        failure.param.as_deref(),
        failure.message.as_deref(),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use crate::transport::ProviderTransport;
    use futures_util::StreamExt;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn credential(secret: Value) -> SubscriptionCredential {
        SubscriptionCredential {
            provider_id: SIGNIN_PROVIDER_ID.into(),
            method_id: "chatgpt-signin".into(),
            version: 1,
            expires_at_unix_s: None,
            account_label: None,
            secret,
        }
    }

    fn test_credential() -> SubscriptionCredential {
        credential(json!({"tokens": {"access_token": "plan-access", "expires_at_unix_s": 1}}))
    }

    /// One canned response: status, extra header lines (each ending in `\r\n`), body.
    struct Canned {
        status: u16,
        headers: &'static str,
        body: &'static str,
    }

    /// Serves one canned response per accepted connection, in order. The
    /// recorded bytes are the most recent request; the count is the number of
    /// requests received.
    async fn stub_sequence(
        responses: Vec<Canned>,
    ) -> (
        String,
        Arc<Mutex<Vec<u8>>>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let output = Arc::clone(&recorded);
        let observed = Arc::clone(&count);
        let task = tokio::spawn(async move {
            for Canned {
                status,
                headers,
                body,
            } in responses
            {
                let (mut socket, _) = listener.accept().await.expect("accept");
                observed.fetch_add(1, Ordering::SeqCst);
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 4096];
                let mut expected = None;
                loop {
                    let n = socket.read(&mut chunk).await.expect("read");
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if expected.is_none() {
                        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&bytes[..end]);
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().ok())
                                        .flatten()
                                })
                                .unwrap_or(0);
                            expected = Some(end + 4 + length);
                        }
                    }
                    if expected.is_some_and(|len| bytes.len() >= len) {
                        break;
                    }
                }
                *output.lock().expect("record") = bytes;
                let response = format!(
                    "HTTP/1.1 {status} Error\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("response");
            }
        });
        (format!("http://{address}"), recorded, count, task)
    }

    async fn stub(
        status: u16,
        body: &'static str,
    ) -> (
        String,
        Arc<Mutex<Vec<u8>>>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        stub_sequence(vec![Canned {
            status,
            headers: "",
            body,
        }])
        .await
    }

    fn recorded_parts(bytes: &[u8]) -> (String, Value) {
        let end = bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("headers");
        (
            String::from_utf8_lossy(&bytes[..end]).into_owned(),
            serde_json::from_slice(&bytes[end + 4..]).expect("json body"),
        )
    }

    #[tokio::test]
    async fn sends_bearer_only_headers() {
        let (base, recorded, _, task) = stub(
            200,
            "event: response.completed\ndata: {\"response\":{}}\n\n",
        )
        .await;
        let provider = PlanResponsesProvider::new(
            test_credential(),
            "gpt-test".into(),
            Some(base),
            ProviderTransport::new(Default::default()).unwrap(),
        )
        .unwrap();
        let mut stream = provider
            .stream("", &[], &[], &Default::default(), None)
            .await
            .unwrap();
        while let Some(item) = stream.next().await {
            let _ = item.unwrap();
        }
        task.await.unwrap();
        let (headers, _) = recorded_parts(&recorded.lock().unwrap());
        assert!(headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case("authorization: Bearer plan-access")));
        assert!(headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case("content-type: application/json")));
        assert!(!headers.to_ascii_lowercase().contains("chatgpt-account-id"));
        assert!(!headers.to_ascii_lowercase().contains("x-openai-fedramp"));
        assert!(!headers.to_ascii_lowercase().contains("originator"));
        assert!(headers.starts_with("POST /responses "));
    }

    #[tokio::test]
    async fn body_has_store_false_stream_true() {
        let (base, recorded, _, task) = stub(
            200,
            "event: response.completed\ndata: {\"response\":{}}\n\n",
        )
        .await;
        let provider = PlanResponsesProvider::new(
            test_credential(),
            "gpt-test".into(),
            Some(base),
            ProviderTransport::new(Default::default()).unwrap(),
        )
        .unwrap();
        let mut stream = provider
            .stream("", &[], &[], &Default::default(), None)
            .await
            .unwrap();
        while let Some(item) = stream.next().await {
            let _ = item.unwrap();
        }
        task.await.unwrap();
        let (_, body) = recorded_parts(&recorded.lock().unwrap());
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "gpt-test");
        assert!(body["input"].is_array());
    }

    #[tokio::test]
    async fn usage_limit_429_is_one_request() {
        let (base, _, count, task) = stub_sequence(vec![Canned {
            status: 429,
            headers: "x-request-id: req_429\r\n",
            body: r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}"#,
        }])
        .await;
        let provider = PlanResponsesProvider::new(
            test_credential(),
            "gpt-test".into(),
            Some(base),
            ProviderTransport::new(crate::factory::transport_config_for(
                crate::providers::Wire::ResponsesPlan,
            ))
            .unwrap(),
        )
        .unwrap();
        let result = provider
            .stream("", &[], &[], &Default::default(), None)
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("429 must be terminal"),
        };
        task.await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        match &error {
            ProviderError::Http {
                status,
                request_id,
                body,
                ..
            } => {
                assert_eq!(*status, 429);
                assert_eq!(request_id.as_deref(), Some("req_429"));
                assert!(body.contains("ChatGPT Settings > Usage"), "body: {body}");
            }
            other => panic!("expected Http error, got {other:?}"),
        }
        assert!(error.to_string().contains("Usage"));
    }

    #[tokio::test]
    async fn responses_plan_503_still_retries() {
        let (base, _, count, task) = stub_sequence(vec![
            Canned {
                status: 503,
                headers: "",
                body: "{}",
            },
            Canned {
                status: 200,
                headers: "",
                body: "event: response.completed\ndata: {\"response\":{}}\n\n",
            },
        ])
        .await;
        let mut config =
            crate::factory::transport_config_for(crate::providers::Wire::ResponsesPlan);
        config.initial_backoff = Duration::from_millis(1);
        config.max_backoff = Duration::from_millis(1);
        let provider = PlanResponsesProvider::new(
            test_credential(),
            "gpt-test".into(),
            Some(base),
            ProviderTransport::new(config).unwrap(),
        )
        .unwrap();
        let mut stream = provider
            .stream("", &[], &[], &Default::default(), None)
            .await
            .expect("503 is retried and the 200 succeeds");
        while let Some(item) = stream.next().await {
            item.expect("retried stream yields no error");
        }
        task.await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn null_tokens_rejected_with_login_hint() {
        let result = PlanResponsesProvider::new(
            credential(json!({"tokens": null})),
            "model".into(),
            None,
            crate::transport::ProviderTransport::new(Default::default()).unwrap(),
        );
        assert!(result
            .unwrap_err()
            .contains("gait auth login openai-signin"));
    }

    #[test]
    fn other_provider_id_rejected() {
        let mut c = credential(json!({"tokens": {"access_token": "secret"}}));
        c.provider_id = "openai".into();
        let result = PlanResponsesProvider::new(
            c,
            "model".into(),
            None,
            crate::transport::ProviderTransport::new(Default::default()).unwrap(),
        );
        assert!(result.unwrap_err().contains("different provider"));
    }

    #[test]
    fn error_mapping_table() {
        // One exact-string assertion per row of the spec's error table.
        assert_eq!(
            map_plan_error(
                Some(403),
                Some("subscription_sharing_user_not_eligible"),
                None,
                None,
                None
            ),
            "not available for this account/workspace"
        );
        let usage = map_plan_error(
            Some(429),
            Some("subscription_sharing_usage_limit_exceeded"),
            None,
            None,
            None,
        );
        assert_eq!(usage, "plan limit reached; see ChatGPT Settings > Usage");
        assert!(usage.contains("Usage") && usage.contains("ChatGPT Settings"));
        assert_eq!(
            map_plan_error(
                Some(503),
                Some("subscription_sharing_usage_unavailable"),
                None,
                None,
                None
            ),
            "plan service temporarily unavailable; retry"
        );
        assert_eq!(
            map_plan_error(
                Some(503),
                Some("subscription_sharing_user_unavailable"),
                None,
                None,
                None
            ),
            "plan service temporarily unavailable; retry"
        );
        assert_eq!(
            map_plan_error(Some(503), None, None, None, None),
            "plan service temporarily unavailable; retry"
        );
        assert_eq!(
            map_plan_error(
                Some(400),
                Some("subscription_sharing_unsupported_capability"),
                Some("vision"),
                None,
                None
            ),
            "unsupported capability: vision"
        );
        assert_eq!(
            map_plan_error(
                Some(403),
                Some("subscription_sharing_route_not_supported"),
                None,
                None,
                None
            ),
            "configuration error: route not supported"
        );
        assert_eq!(
            map_plan_error(Some(403), Some("chatpass_v2_entitlement"), None, None, None),
            "configuration error: ChatGPT plan configuration is not supported"
        );
        assert_eq!(
            map_plan_error(
                Some(401),
                Some("subscription_sharing_invalid_user"),
                None,
                None,
                Some("req_user_1")
            ),
            "invalid subscription user; the next turn refreshes the credential (request id: req_user_1)"
        );
        assert_eq!(
            map_plan_error(Some(401), None, None, None, None),
            "credential expired mid-turn; the next turn refreshes it"
        );
        assert_eq!(
            map_plan_error(Some(400), None, None, Some("detail text"), None),
            "detail text"
        );
    }

    #[test]
    fn unknown_code_falls_back_without_echoing_body() {
        let body = r#"{"error":{"code":"mystery_code"},"extra":"LEAKED-VALUE"}"#;
        let got = map_http_error(400, body, Some("req_unknown"));
        assert_eq!(got, "mystery_code (request id: req_unknown)");
        assert!(!got.contains("LEAKED-VALUE"), "body echoed: {got}");
        assert!(!got.contains('{'), "raw body echoed: {got}");
    }

    #[test]
    fn non_json_error_body_does_not_panic() {
        assert_eq!(
            map_http_error(500, "<html>upstream exploded</html>", None),
            "request failed"
        );
        assert_eq!(
            map_http_error(500, "not json at all", Some("req_text")),
            "request failed (request id: req_text)"
        );
    }

    #[test]
    fn map_http_error_parses_openai_style_code_and_param() {
        let body =
            r#"{"error":{"code":"subscription_sharing_unsupported_capability","param":"vision"}}"#;
        assert_eq!(
            map_http_error(400, body, None),
            "unsupported capability: vision"
        );
    }

    #[test]
    fn map_http_error_handles_detail_body() {
        assert_eq!(
            map_http_error(400, r#"{"detail":"Unsupported model"}"#, Some("req_9")),
            "Unsupported model (request id: req_9)"
        );
    }

    #[test]
    fn mid_stream_usage_limit_error_is_mapped() {
        let failure = crate::responses_wire::ResponsesFailure {
            code: Some("subscription_sharing_usage_limit_exceeded".into()),
            param: None,
            message: Some("Usage".into()),
        };
        assert_eq!(
            plan_failure_format(&failure),
            "plan limit reached; see ChatGPT Settings > Usage"
        );
    }
}
