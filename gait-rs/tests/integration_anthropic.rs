use futures_util::StreamExt;
use gait_core::provider::StreamChunk;
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
    use gait_core::provider::ProviderError;

    // 2 content_block_delta events, then the server drops the connection.
    // No message_stop → the stream is incomplete.
    let server = mock_sse_server(vec![
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hel\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
    ])
    .await;

    let provider = AnthropicProvider::new("k".into(), "claude-x".into(), Some(server.url()), false);
    let mut stream = provider
        .stream("sys", &[], &[], &Default::default(), None)
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
