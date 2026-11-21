//! One `gait.model_call` span per call on the Anthropic and Responses (codex)
//! wires, with the same attributes and guard semantics as the chat wire.
#![cfg(feature = "telemetry")]
use concord_telemetry::testing::{capture_exclusive, span_attr, spans};
use futures_util::StreamExt;
use gait_auth::{ResolvedCredential, SubscriptionCredential};
use gait_provider::factory::load_provider_with_credential_and_params;
use gait_provider::telemetry::{with_call, CallContext};
use gait_types::config::LlmParamsConfig;
use gait_types::types::{Message, MessageContent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn read_request(sock: &mut tokio::net::TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = sock.read(&mut chunk).await.unwrap();
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse().unwrap())
                .unwrap_or(0);
            while buf.len() < i + 4 + len {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            return;
        }
    }
}

#[derive(Clone)]
enum Mode {
    /// SSE segments, each written after its pause in milliseconds.
    Sse(Vec<(u64, String)>),
    Status(u16),
}

fn sse(event: &str, data: serde_json::Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

async fn serve(listener: TcpListener, mode: Mode) {
    loop {
        let (mut sock, _) = listener.accept().await.unwrap();
        read_request(&mut sock).await;
        match &mode {
            Mode::Status(status) => {
                let body = r#"{"error":{"message":"nope"}}"#;
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nretry-after: 0\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
            Mode::Sse(segments) => {
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n")
                    .await;
                for (pause, body) in segments {
                    tokio::time::sleep(std::time::Duration::from_millis(*pause)).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Wire {
    Anthropic,
    Codex,
}

async fn call(wire: Wire, mode: Mode) -> Result<(), String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(listener, mode));
    let (model, env, credential) = match wire {
        Wire::Anthropic => (
            "anthropic/claude-test",
            "ANTHROPIC_BASE_URL",
            ResolvedCredential::ApiKey("k".into()),
        ),
        Wire::Codex => (
            "openai-codex/gpt-test",
            "OPENAI_CODEX_BASE_URL",
            ResolvedCredential::Subscription(SubscriptionCredential {
                provider_id: "openai-codex".into(),
                method_id: "codex-subscription".into(),
                version: 1,
                expires_at_unix_s: None,
                account_label: None,
                secret: serde_json::json!({
                    "id_token": "e30.e30.sig",
                    "access_token": "tok",
                    "refresh_token": "r",
                    "account_id": "acct"
                }),
            }),
        ),
    };
    std::env::set_var(env, format!("http://{addr}/v1"));
    let params = LlmParamsConfig {
        max_tokens: Some(512),
        ..Default::default()
    };
    let provider =
        load_provider_with_credential_and_params(model, Some(credential), &params).unwrap();
    let msgs = vec![Message::User {
        content: MessageContent::Text("hello".into()),
        id: "m1".into(),
        attribution: Vec::new(),
    }];
    let ctx = CallContext {
        session_id: "s-span".into(),
        turn_id: "t1".into(),
        model: model.into(),
        inference: "the-profile".into(),
        reasoning_effort: "low".into(),
    };
    let result = with_call(ctx, async {
        let mut stream = provider
            .stream("sys", &msgs, &[], &params, None)
            .await
            .map_err(|e| e.to_string())?;
        while let Some(chunk) = stream.next().await {
            chunk.map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await;
    server.abort();
    result
}

fn anthropic_ok() -> Mode {
    Mode::Sse(vec![
        (
            0,
            sse(
                "message_start",
                serde_json::json!({"type":"message_start","message":{"model":"claude-served","usage":{
            "input_tokens":10,"cache_read_input_tokens":900,"cache_creation_input_tokens":50}}}),
            ),
        ),
        (
            0,
            sse(
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
        ),
        (
            80,
            sse(
                "content_block_delta",
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
            ),
        ),
        (
            40,
            sse(
                "message_delta",
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":20}}),
            ),
        ),
        (
            0,
            sse("message_stop", serde_json::json!({"type":"message_stop"})),
        ),
    ])
}

fn responses_ok() -> Mode {
    Mode::Sse(vec![
        (
            0,
            sse(
                "response.created",
                serde_json::json!({"response":{"model":"gpt-served"}}),
            ),
        ),
        (
            80,
            sse(
                "response.output_text.delta",
                serde_json::json!({"delta":"hi"}),
            ),
        ),
        (
            40,
            sse(
                "response.completed",
                serde_json::json!({"response":{"model":"gpt-served","status":"completed","usage":{
            "input_tokens":3000,"output_tokens":40,
            "input_tokens_details":{"cached_tokens":2688,"cache_write_tokens":100},
            "output_tokens_details":{"reasoning_tokens":9}}}}),
            ),
        ),
    ])
}

macro_rules! model_call_spans {
    () => {
        spans()
            .into_iter()
            .filter(|s| s.name == "gait.model_call")
            .collect::<Vec<_>>()
    };
}

fn check_common(a: &dyn Fn(&str) -> Option<String>, provider: &str, model: &str, served: &str) {
    assert_eq!(a("gen_ai.operation.name").as_deref(), Some("chat"));
    assert_eq!(a("gen_ai.provider.name").as_deref(), Some(provider));
    assert_eq!(a("gen_ai.request.model").as_deref(), Some(model));
    assert_eq!(a("gen_ai.response.model").as_deref(), Some(served));
    assert_eq!(a("gen_ai.request.max_tokens").as_deref(), Some("512"));
    assert_eq!(a("gait.inference").as_deref(), Some("the-profile"));
    assert_eq!(a("gait.reasoning_effort").as_deref(), Some("low"));
    let ttft: f64 = a("gait.time_to_first_token_ms")
        .expect("ttft")
        .parse()
        .unwrap();
    let duration: f64 = a("gait.duration_ms").expect("duration").parse().unwrap();
    assert!(
        ttft >= 70.0,
        "first token came after the 80ms pause: {ttft}"
    );
    assert!(duration >= ttft, "duration {duration} >= ttft {ttft}");
    // gait's own timing is authoritative: no server-side timing is invented.
    assert!(a("gait.server.time_to_first_token_ms").is_none());
    assert!(a("gait.server.generation_tokens_per_second").is_none());
    assert!(a("error.class").is_none());
}

#[tokio::test]
async fn an_anthropic_call_emits_one_span_with_usage_and_timing() {
    let _env = ENV.lock().await;
    let _capture = capture_exclusive();
    call(Wire::Anthropic, anthropic_ok()).await.unwrap();
    let found = model_call_spans!();
    assert_eq!(found.len(), 1, "one span per model call");
    let s = &found[0];
    let a = |k: &str| span_attr(s, k);
    check_common(&a, "anthropic", "claude-test", "claude-served");
    // input includes the cached and cache-written tokens, as on the chat wire.
    assert_eq!(a("gen_ai.usage.input_tokens").as_deref(), Some("960"));
    assert_eq!(a("gen_ai.usage.output_tokens").as_deref(), Some("20"));
    assert_eq!(a("gait.usage.cached_tokens").as_deref(), Some("900"));
    assert_eq!(a("gait.usage.cache_write_tokens").as_deref(), Some("50"));
    assert_eq!(
        a("gen_ai.response.finish_reasons").as_deref(),
        Some("end_turn")
    );
    // Anthropic reports no reasoning subset of output tokens.
    assert!(
        a("gait.usage.reasoning_tokens").is_none()
            || a("gait.usage.reasoning_tokens").as_deref() == Some("0")
    );
}

#[tokio::test]
async fn a_responses_call_emits_one_span_with_usage_and_timing() {
    let _env = ENV.lock().await;
    let _capture = capture_exclusive();
    call(Wire::Codex, responses_ok()).await.unwrap();
    let found = model_call_spans!();
    assert_eq!(found.len(), 1, "one span per model call");
    let s = &found[0];
    let a = |k: &str| span_attr(s, k);
    check_common(&a, "openai-codex", "gpt-test", "gpt-served");
    assert_eq!(a("gen_ai.usage.input_tokens").as_deref(), Some("3000"));
    assert_eq!(a("gen_ai.usage.output_tokens").as_deref(), Some("40"));
    assert_eq!(a("gait.usage.cached_tokens").as_deref(), Some("2688"));
    assert_eq!(a("gait.usage.cache_write_tokens").as_deref(), Some("100"));
    assert_eq!(a("gait.usage.reasoning_tokens").as_deref(), Some("9"));
    assert_eq!(
        a("gen_ai.response.finish_reasons").as_deref(),
        Some("completed")
    );
}

async fn assert_429(wire: Wire) {
    let _capture = capture_exclusive();
    assert!(call(wire, Mode::Status(429)).await.is_err());
    let found = model_call_spans!();
    assert_eq!(found.len(), 1, "retries stay inside the one call span");
    let s = &found[0];
    assert!(
        span_attr(s, "error.class").is_some(),
        "attrs: {:?}",
        s.attributes
    );
    assert!(span_attr(s, "error.type").is_some());
    assert_eq!(span_attr(s, "http_status").as_deref(), Some("429"));
}

#[tokio::test]
async fn an_anthropic_429_carries_error_class_and_status() {
    let _env = ENV.lock().await;
    assert_429(Wire::Anthropic).await;
}

#[tokio::test]
async fn a_responses_429_carries_error_class_and_status() {
    let _env = ENV.lock().await;
    assert_429(Wire::Codex).await;
}
