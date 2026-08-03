use futures_util::StreamExt;
use gait_auth::codex::CODEX_PROVIDER_ID;
use gait_auth::SubscriptionCredential;
use gait_provider::anthropic::AnthropicProvider;
use gait_provider::codex::CodexResponsesProvider;
use gait_provider::openai::OpenAIProvider;
use gait_provider::provider::{Provider, ProviderError, StreamChunk};
use gait_provider::providers::Wire;
use gait_provider::transport::{SseTransport, TransportConfig};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SECRET: &str = "transport-conformance-secret";

#[derive(Clone)]
enum Script {
    Status {
        status: u16,
        body: String,
        retry_after: Option<u64>,
    },
    Sse {
        body: String,
        hold_open: Duration,
    },
}

struct ScriptedServer {
    base_url: String,
    request_count: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedServer {
    async fn start(scripts: Vec<Script>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let request_count = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed_count = request_count.clone();
        let observed_requests = requests.clone();

        tokio::spawn(async move {
            for script in scripts {
                let (socket, _) = listener.accept().await.expect("accept scripted request");
                observed_count.fetch_add(1, Ordering::SeqCst);
                let observed_requests = observed_requests.clone();
                tokio::spawn(async move {
                    serve_response(socket, script, observed_requests).await;
                });
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            request_count,
            requests,
        }
    }

    fn count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }

    fn captured_requests(&self) -> Vec<String> {
        self.requests.lock().expect("request capture").clone()
    }
}

async fn serve_response(mut socket: TcpStream, script: Script, requests: Arc<Mutex<Vec<String>>>) {
    let request = read_request(&mut socket).await;
    requests.lock().expect("request capture").push(request);

    match script {
        Script::Status {
            status,
            body,
            retry_after,
        } => {
            let reason = match status {
                400 => "Bad Request",
                429 => "Too Many Requests",
                503 => "Service Unavailable",
                _ => "Error",
            };
            let retry_header = retry_after
                .map(|seconds| format!("Retry-After: {seconds}\r\n"))
                .unwrap_or_default();
            let headers = format!(
                "HTTP/1.1 {status} {reason}\r\n{retry_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write status headers");
            socket
                .write_all(body.as_bytes())
                .await
                .expect("write status body");
        }
        Script::Sse { body, hold_open } => {
            let content_length = if hold_open.is_zero() {
                format!("Content-Length: {}\r\n", body.len())
            } else {
                String::new()
            };
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n{content_length}Connection: close\r\n\r\n"
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write SSE headers");
            socket
                .write_all(body.as_bytes())
                .await
                .expect("write SSE body");
            socket.flush().await.expect("flush SSE body");
            if !hold_open.is_zero() {
                tokio::time::sleep(hold_open).await;
            }
        }
    }
    let _ = socket.shutdown().await;
}

async fn read_request(socket: &mut TcpStream) -> String {
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
            if let Some(header_end) = find_header_end(&bytes) {
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
    String::from_utf8_lossy(&bytes).into_owned()
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn test_config() -> TransportConfig {
    TransportConfig {
        connect_timeout: Duration::from_millis(100),
        ttfb_timeout: Duration::from_millis(30),
        idle_timeout: Duration::from_millis(30),
        absolute_cap: Duration::from_secs(2),
        max_attempts: 2,
        initial_backoff: Duration::ZERO,
        max_backoff: Duration::ZERO,
    }
}

fn credential() -> SubscriptionCredential {
    SubscriptionCredential {
        provider_id: CODEX_PROVIDER_ID.into(),
        method_id: "codex-subscription".into(),
        version: 1,
        expires_at_unix_s: None,
        account_label: None,
        secret: json!({
            "id_token": "e30.e30.sig",
            "access_token": SECRET,
            "refresh_token": "refresh-test",
            "account_id": "acct-test"
        }),
    }
}

fn provider_for(wire: Wire, base_url: &str) -> Box<dyn Provider> {
    let transport = SseTransport::new(test_config()).expect("test transport");
    match wire {
        Wire::Anthropic => Box::new(AnthropicProvider::new(
            SECRET.into(),
            "claude-test".into(),
            Some(base_url.into()),
            false,
            transport,
        )),
        Wire::OpenAI => Box::new(OpenAIProvider::new(
            SECRET.into(),
            "gpt-test".into(),
            Some(base_url.into()),
            transport,
        )),
        Wire::CodexResponses => Box::new(
            CodexResponsesProvider::new(
                credential(),
                "gpt-test".into(),
                Some(base_url.into()),
                transport,
            )
            .expect("Codex provider"),
        ),
    }
}

fn complete_sse(wire: Wire) -> String {
    match wire {
        Wire::Anthropic => {
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into()
        }
        Wire::OpenAI => "data: [DONE]\n\n".into(),
        Wire::CodexResponses => "event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n".into(),
    }
}

fn partial_sse(wire: Wire) -> String {
    match wire {
        Wire::Anthropic => "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n".into(),
        Wire::OpenAI => "data: {\"choices\":[{\"delta\":{\"content\":\"hi\",\"tool_calls\":null,\"reasoning_content\":null},\"finish_reason\":null}],\"usage\":null}\n\n".into(),
        Wire::CodexResponses => "event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n".into(),
    }
}

async fn invoke(provider: &dyn Provider) -> Result<Vec<StreamChunk>, ProviderError> {
    let mut stream = provider
        .stream("", &[], &[], &Default::default(), None)
        .await?;
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk?);
    }
    Ok(chunks)
}

fn assert_one_done(chunks: &[StreamChunk]) {
    assert_eq!(
        chunks
            .iter()
            .filter(|chunk| matches!(chunk, StreamChunk::Done))
            .count(),
        1,
        "terminal marker must emit exactly one Done chunk"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_production_sse_wire_obeys_transport_contract() {
    for &wire in Wire::ALL {
        let server = ScriptedServer::start(vec![
            Script::Status {
                status: 503,
                body: "retry".into(),
                retry_after: Some(0),
            },
            Script::Sse {
                body: complete_sse(wire),
                hold_open: Duration::ZERO,
            },
        ])
        .await;
        let chunks = invoke(provider_for(wire, &server.base_url).as_ref())
            .await
            .expect("retryable pre-event failure should recover");
        assert_one_done(&chunks);
        assert_eq!(server.count(), 2, "{wire:?}: retry once");

        let server = ScriptedServer::start(vec![Script::Status {
            status: 400,
            body: "bad request".into(),
            retry_after: None,
        }])
        .await;
        let error = invoke(provider_for(wire, &server.base_url).as_ref())
            .await
            .expect_err("400 must be terminal");
        assert!(matches!(error, ProviderError::Http { status: 400, .. }));
        assert_eq!(server.count(), 1, "{wire:?}: 400 is not retried");

        let server = ScriptedServer::start(vec![
            Script::Status {
                status: 503,
                body: "retry one".into(),
                retry_after: Some(0),
            },
            Script::Status {
                status: 503,
                body: "retry two".into(),
                retry_after: Some(0),
            },
        ])
        .await;
        let error = invoke(provider_for(wire, &server.base_url).as_ref())
            .await
            .expect_err("503 should exhaust the configured attempts");
        assert!(matches!(
            error,
            ProviderError::Http {
                status: 503,
                retry_after: Some(delay),
                ..
            } if delay.is_zero()
        ));
        assert_eq!(server.count(), test_config().max_attempts as usize);

        let server = ScriptedServer::start(vec![
            Script::Sse {
                body: String::new(),
                hold_open: Duration::from_millis(100),
            },
            Script::Sse {
                body: complete_sse(wire),
                hold_open: Duration::ZERO,
            },
        ])
        .await;
        let chunks = invoke(provider_for(wire, &server.base_url).as_ref())
            .await
            .expect("zero-event TTFB timeout should retry");
        assert_one_done(&chunks);
        assert_eq!(server.count(), 2, "{wire:?}: zero-event timeout retries");

        let server = ScriptedServer::start(vec![Script::Sse {
            body: partial_sse(wire),
            hold_open: Duration::from_millis(100),
        }])
        .await;
        let error = invoke(provider_for(wire, &server.base_url).as_ref())
            .await
            .expect_err("post-event idle timeout must be terminal");
        assert!(matches!(
            error,
            ProviderError::Timeout {
                phase: gait_provider::provider::TimeoutPhase::Idle
            }
        ));
        assert_eq!(server.count(), 1, "{wire:?}: idle timeout is not replayed");

        let server = ScriptedServer::start(vec![Script::Sse {
            body: partial_sse(wire),
            hold_open: Duration::ZERO,
        }])
        .await;
        let error = invoke(provider_for(wire, &server.base_url).as_ref())
            .await
            .expect_err("partial clean EOF must be incomplete");
        assert!(matches!(error, ProviderError::StreamIncomplete { .. }));
        assert_eq!(
            server.count(),
            1,
            "{wire:?}: incomplete stream is not replayed"
        );

        let server = ScriptedServer::start(vec![Script::Sse {
            body: complete_sse(wire),
            hold_open: Duration::ZERO,
        }])
        .await;
        let chunks = invoke(provider_for(wire, &server.base_url).as_ref())
            .await
            .expect("normal terminal marker");
        assert_one_done(&chunks);
        assert_eq!(server.count(), 1);

        let reflected = format!("server reflected {SECRET} {}", "x".repeat(80 * 1024));
        let server = ScriptedServer::start(vec![Script::Status {
            status: 400,
            body: reflected,
            retry_after: None,
        }])
        .await;
        let error = invoke(provider_for(wire, &server.base_url).as_ref())
            .await
            .expect_err("reflected credential response should fail");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(SECRET), "{wire:?}: credential leaked");
        let ProviderError::Http { body, .. } = error else {
            panic!("expected HTTP error")
        };
        assert!(body.len() <= 64 * 1024, "error body must be bounded");
        let requests = server.captured_requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].contains(SECRET),
            "test must prove the credential was sent before checking redaction"
        );
    }
}
