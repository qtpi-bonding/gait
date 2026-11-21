//! Usage survives every shape an OpenAI-compatible server (oMLX, lmstudio,
//! OpenAI itself) puts it in. The consumer here stops at `Done`, like the turn
//! loop (`src/turn/stream.rs`) does, so usage that arrives after the finish
//! chunk is lost unless the provider holds `Done` back.
use futures_util::StreamExt;
use gait_provider::factory::load_provider_with_credential_and_params;
use gait_provider::provider::StreamChunk;
use gait_types::config::LlmParamsConfig;
use gait_types::types::{Message, MessageContent, Usage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const FINISH: &str =
    r#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#;

async fn serve(listener: TcpListener, sse: String, hold_open: bool) {
    let (mut sock, _) = listener.accept().await.unwrap();
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
            break;
        }
    }
    if hold_open {
        // No content-length and no close: the body ends only when the socket does.
        let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n{sse}");
        let _ = sock.write_all(resp.as_bytes()).await;
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        return;
    }
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
        sse.len()
    );
    let _ = sock.write_all(resp.as_bytes()).await;
}

/// The usage a turn would total for this SSE body: stops reading at `Done`.
async fn usage_for(lines: &[&str]) -> (u32, u32, u32) {
    run(lines, false).await.0
}

/// (usage, tool calls completed, Done count) for one served body.
async fn run(lines: &[&str], hold_open: bool) -> ((u32, u32, u32), usize, usize) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    std::env::set_var("LMSTUDIO_BASE_URL", format!("http://{addr}/v1"));
    let sse: String = lines.iter().map(|l| format!("{l}\n\n")).collect();
    let server = tokio::spawn(serve(listener, sse, hold_open));
    let params = LlmParamsConfig::default();
    let provider =
        load_provider_with_credential_and_params("lmstudio/some-model", None, &params).unwrap();
    let msgs = vec![Message::User {
        content: MessageContent::Text("hello".into()),
        id: "m1".into(),
        attribution: Vec::new(),
    }];
    let mut stream = provider
        .stream("sys", &msgs, &[], &params, None)
        .await
        .unwrap();
    let mut total = Usage::default();
    let (mut tools, mut dones) = (0, 0);
    while let Some(chunk) = stream.next().await {
        match chunk.unwrap() {
            StreamChunk::StepUsage(u) => total = total.add(&u),
            StreamChunk::ToolCallComplete { .. } => tools += 1,
            StreamChunk::Done => {
                dones += 1;
                break;
            }
            _ => {}
        }
    }
    server.abort();
    (
        (
            total.input_tokens,
            total.output_tokens,
            total.cache_read_tokens,
        ),
        tools,
        dones,
    )
}

fn usage(input: u32, output: u32, cached: u32) -> (u32, u32, u32) {
    (input, output, cached)
}

// The base URL is process-global environment: tests take the ENV lock.
#[tokio::test]
async fn usage_is_recorded_whatever_shape_the_server_sends_it_in() {
    let _env = ENV.lock().await;
    let std_usage = r#"data: {"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":7}}}"#;

    // OpenAI's own shape: finish chunk, then a usage chunk with empty choices, then [DONE].
    assert_eq!(
        usage_for(&[FINISH, std_usage, "data: [DONE]"]).await,
        usage(60, 20, 40),
        "usage after finish, choices []"
    );
    // choices absent entirely.
    let no_choices = r#"data: {"usage":{"prompt_tokens":10,"completion_tokens":3}}"#;
    assert_eq!(
        usage_for(&[FINISH, no_choices, "data: [DONE]"]).await,
        usage(10, 3, 0),
        "choices absent"
    );
    // usage on the finish chunk itself.
    let on_finish = r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#;
    assert_eq!(
        usage_for(&[on_finish, "data: [DONE]"]).await,
        usage(10, 3, 0),
        "usage on the finish chunk"
    );
    // null / missing completion_tokens must not drop the prompt side.
    let null_completion =
        r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":null}}"#;
    assert_eq!(
        usage_for(&[FINISH, null_completion, "data: [DONE]"]).await,
        usage(10, 0, 0),
        "null completion_tokens"
    );
    let no_completion = r#"data: {"choices":[],"usage":{"prompt_tokens":10}}"#;
    assert_eq!(
        usage_for(&[FINISH, no_completion, "data: [DONE]"]).await,
        usage(10, 0, 0),
        "missing completion_tokens"
    );
    // extra non-standard fields next to the standard ones.
    let extra = r#"data: {"id":"x","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13,"peak_memory":1.5,"time_to_first_token":0.2,"generation_tokens_per_second":41.0,"prompt_tokens_details":{"cached_tokens":null}},"timings":{"x":1}}"#;
    assert_eq!(
        usage_for(&[FINISH, extra, "data: [DONE]"]).await,
        usage(10, 3, 0),
        "extra fields"
    );
    // [DONE] is the terminator: a chunk after it is not read.
    assert_eq!(
        usage_for(&[FINISH, "data: [DONE]", std_usage]).await,
        usage(0, 0, 0),
        "usage after [DONE] is ignored"
    );
    // No [DONE] at all, the connection just closes after the usage chunk.
    assert_eq!(
        usage_for(&[FINISH, std_usage]).await,
        usage(60, 20, 40),
        "no [DONE]"
    );
}

#[tokio::test]
async fn repeated_finish_and_usage_chunks_count_once_and_a_held_open_socket_does_not_stall() {
    let _env = ENV.lock().await;
    let tool = r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"ls","arguments":"{}"}}]},"finish_reason":null}]}"#;
    let finish = r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#;
    let again = r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#;
    let (usage, tools, dones) = run(&[tool, finish, again, "data: [DONE]"], false).await;
    assert_eq!(
        (usage, tools, dones),
        ((10, 3, 0), 1, 1),
        "usage on two chunks counts once; the tool call completes once"
    );

    let started = std::time::Instant::now();
    let (usage, _, dones) = run(
        &[
            FINISH,
            r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#,
        ],
        true,
    )
    .await;
    assert_eq!((usage, dones), ((10, 3, 0), 1));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "a server that never closes must not hold Done for long"
    );
}

#[tokio::test]
async fn done_ends_the_stream_at_once_even_if_the_server_keeps_the_socket_open() {
    let _env = ENV.lock().await;
    let usage_chunk = r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#;
    let started = std::time::Instant::now();
    let (usage, _, dones) = run(&[FINISH, usage_chunk, "data: [DONE]"], true).await;
    assert_eq!((usage, dones), ((10, 3, 0), 1));
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "[DONE] is the terminator: took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_finish_chunk_that_already_carried_usage_does_not_wait_for_a_trailer() {
    let _env = ENV.lock().await;
    let on_finish = r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#;
    let started = std::time::Instant::now();
    let (usage, _, dones) = run(&[on_finish], true).await;
    assert_eq!((usage, dones), ((10, 3, 0), 1));
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "took {:?}",
        started.elapsed()
    );
}
