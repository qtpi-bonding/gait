//! One `gait.model_call` span per model call: the gen_ai attributes, token
//! counts, timing computed from the stream, and the error class on failure.
#![cfg(feature = "telemetry")]
use concord_telemetry::testing::{capture_exclusive, span_attr, spans};
use futures_util::StreamExt;
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

/// Answers every request: a streamed body whose first token comes after a
/// pause, or an HTTP error status.
#[derive(Clone)]
enum Mode {
    Ok,
    Status(u16),
    /// First chunk, then silence: the client drops the stream mid-body.
    Stall,
    /// This exact SSE body, then close.
    Raw(String),
}

async fn serve(listener: TcpListener, mode: Mode) {
    loop {
        let (mut sock, _) = listener.accept().await.unwrap();
        read_request(&mut sock).await;
        if let Mode::Raw(body) = &mode {
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            continue;
        }
        if let Mode::Status(status) = mode {
            let body = r#"{"error":{"message":"nope"}}"#;
            let resp = format!(
                "HTTP/1.1 {status} X\r\nretry-after: 0\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            continue;
        }
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"id\":\"c\",\"model\":\"served-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n")
            .await;
        if matches!(mode, Mode::Stall) {
            let _ = sock
                .write_all(
                    b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
                )
                .await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let _ = sock
            .write_all(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n")
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let _ = sock
            .write_all(b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20,\"prompt_tokens_details\":{\"cached_tokens\":40},\"completion_tokens_details\":{\"reasoning_tokens\":7},\"time_to_first_token\":0.25,\"generation_tokens_per_second\":41.5}}\n\ndata: [DONE]\n\n")
            .await;
    }
}

async fn call(mode: Mode) -> Result<(), String> {
    call_with(mode, false).await
}

/// `drop_after_first`: read one chunk past the opening one, then drop the stream.
async fn call_with(mode: Mode, drop_after_first: bool) -> Result<(), String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    std::env::set_var("LMSTUDIO_BASE_URL", format!("http://{addr}/v1"));
    let server = tokio::spawn(serve(listener, mode));
    let params = LlmParamsConfig {
        temperature: Some(0.5),
        top_p: Some(0.9),
        max_tokens: Some(512),
        ..Default::default()
    };
    let provider =
        load_provider_with_credential_and_params("lmstudio/some-model", None, &params).unwrap();
    let msgs = vec![Message::User {
        content: MessageContent::Text("hello".into()),
        id: "m1".into(),
        attribution: Vec::new(),
    }];
    let ctx = CallContext {
        session_id: "s-span".into(),
        turn_id: "t1".into(),
        model: "lmstudio/some-model".into(),
        inference: "studio-glm".into(),
        reasoning_effort: "low".into(),
    };
    let result = with_call(ctx, async {
        let mut stream = provider
            .stream("sys", &msgs, &[], &params, None)
            .await
            .map_err(|e| e.to_string())?;
        let mut seen = 0;
        while let Some(chunk) = stream.next().await {
            chunk.map_err(|e| e.to_string())?;
            seen += 1;
            if drop_after_first && seen == 1 {
                break;
            }
        }
        drop(stream);
        Ok(())
    })
    .await;
    server.abort();
    result
}

macro_rules! model_call_spans {
    () => {
        spans()
            .into_iter()
            .filter(|s| s.name == "gait.model_call")
            .collect::<Vec<_>>()
    };
}

#[tokio::test]
async fn a_call_emits_one_gen_ai_span_with_usage_timing_and_who_it_is_for() {
    let _env = ENV.lock().await;
    let _capture = capture_exclusive();
    call(Mode::Ok).await.unwrap();
    let found = model_call_spans!();
    assert_eq!(found.len(), 1, "one span per model call");
    let s = &found[0];
    let a = |k: &str| span_attr(s, k);
    assert_eq!(a("gen_ai.operation.name").as_deref(), Some("chat"));
    assert_eq!(a("gen_ai.provider.name").as_deref(), Some("lmstudio"));
    assert_eq!(a("gen_ai.request.model").as_deref(), Some("some-model"));
    assert_eq!(a("gen_ai.response.model").as_deref(), Some("served-model"));
    assert_eq!(a("gen_ai.request.temperature").as_deref(), Some("0.5"));
    assert_eq!(a("gen_ai.request.top_p").as_deref(), Some("0.9"));
    assert_eq!(a("gen_ai.request.max_tokens").as_deref(), Some("512"));
    assert_eq!(a("gait.inference").as_deref(), Some("studio-glm"));
    assert_eq!(a("gait.reasoning_effort").as_deref(), Some("low"));
    // gen_ai input tokens include the cached ones; output includes reasoning.
    assert_eq!(a("gen_ai.usage.input_tokens").as_deref(), Some("100"));
    assert_eq!(a("gen_ai.usage.output_tokens").as_deref(), Some("20"));
    assert_eq!(a("gait.usage.cached_tokens").as_deref(), Some("40"));
    assert_eq!(a("gait.usage.reasoning_tokens").as_deref(), Some("7"));
    assert_eq!(a("gait.usage.cache_write_tokens").as_deref(), Some("0"));
    assert_eq!(a("gen_ai.response.finish_reasons").as_deref(), Some("stop"));
    let ttft: f64 = a("gait.time_to_first_token_ms")
        .expect("ttft")
        .parse()
        .unwrap();
    let duration: f64 = a("gait.duration_ms").expect("duration").parse().unwrap();
    assert!(
        ttft >= 70.0,
        "first token came after the 80ms pause: {ttft}"
    );
    assert!(
        duration >= ttft + 30.0,
        "duration {duration} covers the stream after ttft {ttft}"
    );
    let tps: f64 = a("gait.output_tokens_per_second")
        .expect("tps")
        .parse()
        .unwrap();
    assert!(tps > 0.0);
    // The server's own numbers ride along, never replacing gait's.
    assert_eq!(
        a("gait.server.time_to_first_token_ms").as_deref(),
        Some("250")
    );
    assert_eq!(
        a("gait.server.generation_tokens_per_second").as_deref(),
        Some("41.5")
    );
    assert!(a("error.class").is_none());
}

#[tokio::test]
async fn a_failing_call_carries_its_error_class() {
    let _env = ENV.lock().await;
    let _capture = capture_exclusive();
    assert!(call(Mode::Status(400)).await.is_err());
    let found = model_call_spans!();
    assert_eq!(found.len(), 1);
    let s = &found[0];
    assert!(
        span_attr(s, "error.class").is_some(),
        "attrs: {:?}",
        s.attributes
    );
    assert_eq!(span_attr(s, "http_status").as_deref(), Some("400"));
    assert!(span_attr(s, "error.type").is_some());
    assert_eq!(
        span_attr(s, "gen_ai.request.model").as_deref(),
        Some("some-model")
    );
}

#[tokio::test]
async fn a_429_carries_error_class_and_type_on_the_call_span() {
    let _env = ENV.lock().await;
    let _capture = capture_exclusive();
    assert!(call(Mode::Status(429)).await.is_err());
    let found = model_call_spans!();
    assert_eq!(found.len(), 1, "retries stay inside the one call span");
    let s = &found[0];
    assert!(
        span_attr(s, "error.class").is_some(),
        "attrs: {:?}",
        s.attributes
    );
    assert_eq!(
        span_attr(s, "error.type").as_deref(),
        span_attr(s, "error.code").as_deref()
    );
    assert!(span_attr(s, "error.type").is_some());
    assert_eq!(span_attr(s, "http_status").as_deref(), Some("429"));
}

#[tokio::test]
async fn dropping_the_stream_mid_body_still_ends_the_span() {
    let _env = ENV.lock().await;
    let _capture = capture_exclusive();
    call_with(Mode::Stall, true).await.unwrap();
    let found = model_call_spans!();
    assert_eq!(found.len(), 1, "the dropped call's span was exported");
    let s = &found[0];
    assert!(
        span_attr(s, "gait.duration_ms").is_some(),
        "attrs: {:?}",
        s.attributes
    );
    assert!(span_attr(s, "gait.time_to_first_token_ms").is_some());
}

#[tokio::test]
async fn a_real_omlx_tail_records_usage_and_server_timings() {
    let _env = ENV.lock().await;
    let _capture = capture_exclusive();
    let tail = include_str!("fixtures/omlx_sse_tail.txt");
    let body: String = tail
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| format!("{l}\n\n"))
        .collect();
    call(Mode::Raw(body)).await.unwrap();
    let found = model_call_spans!();
    assert_eq!(found.len(), 1);
    let s = &found[0];
    let a = |k: &str| span_attr(s, k);
    assert_eq!(a("gen_ai.usage.input_tokens").as_deref(), Some("18"));
    assert_eq!(a("gen_ai.usage.output_tokens").as_deref(), Some("7"));
    assert_eq!(a("gait.usage.cached_tokens").as_deref(), Some("16"));
    assert_eq!(a("gait.usage.reasoning_tokens").as_deref(), Some("1"));
    assert_eq!(
        a("gen_ai.response.model").as_deref(),
        Some("GLM-5.3-Flash-oQ4e-mtp")
    );
    assert_eq!(a("gen_ai.response.finish_reasons").as_deref(), Some("stop"));
    assert_eq!(
        a("gait.server.time_to_first_token_ms").as_deref(),
        Some("80")
    );
    assert_eq!(
        a("gait.server.generation_tokens_per_second").as_deref(),
        Some("52.15")
    );
}
