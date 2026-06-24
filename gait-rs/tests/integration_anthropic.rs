use futures_util::StreamExt;
use gait_core::provider::{Provider, ProviderError, StreamChunk};
use gait_core::{load_provider, Message, MessageContent};

#[tokio::test]
async fn anthropic_streams_hello() {
    let key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            eprintln!("ANTHROPIC_API_KEY not set, skipping");
            return;
        }
    };

    let provider = load_provider("anthropic/claude-haiku-4-5", Some(key)).unwrap();
    let messages = vec![Message::User {
        id: String::new(),
        content: MessageContent::Text("Say exactly: hello world".into()),
    }];

    let mut stream = provider
        .stream(
            "You are helpful.",
            &messages,
            &[],
            &Default::default(),
            None,
        )
        .await
        .unwrap();
    let mut text = String::new();
    let mut output_tokens = 0u32;
    let mut input_tokens = 0u32;
    while let Some(chunk) = stream.next().await {
        match chunk.unwrap() {
            StreamChunk::Text(t) => text.push_str(&t),
            StreamChunk::StepUsage(u) => {
                output_tokens += u.output_tokens;
                input_tokens += u.input_tokens;
            }
            StreamChunk::Done => break,
            _ => {}
        }
    }
    assert!(text.to_lowercase().contains("hello"));
    assert!(output_tokens > 0);
    // Input tokens tracked from MessageStart (C5 fix)
    assert!(input_tokens > 0);
}

// ── Mock SSE server (Task 1) ───────────────────────────────────────────────
// Spins a one-shot HTTP/1.1 server on 127.0.0.1 that writes the given bytes
// as the response body and immediately drops the connection. Returns the URL
// the test client should hit (e.g. `http://127.0.0.1:54321/v1`).
async fn mock_sse_server(frames: Vec<&'static str>) -> MockSse {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{}/v1", addr);

    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            // Bare-minimum HTTP/1.1 headers; we deliberately omit Content-Length
            // and close the connection right after the body so the test never has
            // to wait for an EOF that never comes.
            let header =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            let _ = sock.write_all(header.as_bytes()).await;
            for frame in frames {
                let _ = sock.write_all(frame.as_bytes()).await;
            }
            let _ = sock.shutdown().await;
        }
    });

    MockSse { url }
}

struct MockSse {
    url: String,
}

impl MockSse {
    fn url(&self) -> String {
        self.url.clone()
    }
}

#[tokio::test]
async fn stream_ending_without_message_stop_yields_incomplete() {
    use gait_core::anthropic::AnthropicProvider;

    // 2 content_block_delta events, then the server drops the connection.
    // No message_stop → the stream is incomplete.
    let server = mock_sse_server(vec![
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hel\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
    ])
    .await;

    let provider = AnthropicProvider::new(
        "k".into(),
        "claude-x".into(),
        Some(server.url()),
        false,
        gait_core::transport::TransportConfig::default(),
    );
    let mut stream = provider
        .stream(
            "sys",
            &[],
            &[],
            &gait_core::config::LlmParamsConfig::default(),
            None,
        )
        .await
        .unwrap();

    let mut last = None;
    while let Some(item) = stream.next().await {
        last = Some(item);
    }
    assert!(
        matches!(
            last,
            Some(Err(ProviderError::StreamIncomplete { received_bytes })) if received_bytes == 5
        ),
        "expected StreamIncomplete {{ received_bytes: 5 }}, got {:?}",
        last
    );
}

/// Task 6 — usage accounting on mid-stream failure.
///
/// A stream that emits `StepUsage{input:100}` then fails with `StreamIncomplete`
/// must surface the error **and** account the partial usage exactly once.
///
/// Current behavior (v1): the caller (turn/mod.rs) accumulates usage into
/// `total_usage` before the stream error propagates. If the turn is retried at
/// the orchestration spine, those 100 input tokens will be counted again.
///
/// KNOWN: partial usage counted once here; re-counted on spine re-invoke if the
/// caller retries. A usage-rollback mechanism belongs in a follow-up task once
/// we decide whether to debit per-attempt or per-successful-turn only.
///
/// This test pins the *current* (single-pass) behavior so a future rollback
/// implementation is forced to update this assertion deliberately.
#[tokio::test]
async fn partial_usage_then_incomplete_is_accounted_once() {
    use gait_core::anthropic::AnthropicProvider;

    // Emits a MessageStart (100 input tokens) + one text delta, then drops the
    // connection without a message_stop — triggering StreamIncomplete.
    let server = mock_sse_server(vec![
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"x\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-x\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":100,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    ])
    .await;

    let provider = AnthropicProvider::new(
        "k".into(),
        "claude-x".into(),
        Some(server.url()),
        false,
        gait_core::transport::TransportConfig::default(),
    );
    let mut stream = provider
        .stream(
            "sys",
            &[],
            &[],
            &gait_core::config::LlmParamsConfig::default(),
            None,
        )
        .await
        .unwrap();

    let mut input_tokens_seen: u32 = 0;
    let mut got_incomplete = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamChunk::StepUsage(u)) => input_tokens_seen += u.input_tokens,
            Err(ProviderError::StreamIncomplete { .. }) => {
                got_incomplete = true;
                break;
            }
            _ => {}
        }
    }

    assert!(got_incomplete, "expected StreamIncomplete error");
    // KNOWN: 100 tokens from MessageStart are counted once per stream invocation.
    // On spine-level retry these tokens would be counted again — see doc comment.
    assert_eq!(
        input_tokens_seen, 100,
        "partial usage must be counted exactly once per stream pass"
    );
}
