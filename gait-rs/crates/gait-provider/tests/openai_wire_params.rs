//! The chat request an OpenAI-wire provider other than `openai` itself (here
//! lmstudio, via LMSTUDIO_BASE_URL) sends carries the `llm_params.openai`
//! fields and the universal ones, and omits them when unset.
use gait_provider::factory::load_provider_with_credential_and_params;
use gait_types::config::{LlmParamsConfig, OpenAILlmParamsConfig};
use gait_types::types::{Message, MessageContent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves one request, returns the POST body it received.
async fn capture_one_request(listener: TcpListener) -> String {
    let (mut sock, _) = listener.accept().await.unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let body_start = loop {
        let n = sock.read(&mut chunk).await.unwrap();
        assert!(n > 0, "connection closed before the headers ended");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map(|v| v.trim().parse().unwrap())
        .expect("content-length");
    while buf.len() < body_start + len {
        let n = sock.read(&mut chunk).await.unwrap();
        assert!(n > 0, "connection closed before the body ended");
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8(buf[body_start..body_start + len].to_vec()).unwrap();
    let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
        sse.len()
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    body
}

async fn request_body(params: LlmParamsConfig) -> serde_json::Value {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    std::env::set_var("LMSTUDIO_BASE_URL", format!("http://{addr}/v1"));
    let server = tokio::spawn(capture_one_request(listener));
    let provider = load_provider_with_credential_and_params("lmstudio/some-model", None, &params)
        .expect("lmstudio loads without a key");
    let msgs = vec![Message::User {
        content: MessageContent::Text("hello".into()),
        id: "m1".into(),
        attribution: Vec::new(),
    }];
    let _ = provider.stream("sys", &msgs, &[], &params, None).await;
    let body = tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .expect("fake server got a request")
        .unwrap();
    serde_json::from_str(&body).unwrap()
}

// One test, run in order: the base URL is process-global environment.
#[tokio::test]
async fn lmstudio_chat_request_carries_openai_params_and_omits_them_when_unset() {
    let set = LlmParamsConfig {
        max_tokens: Some(512),
        temperature: Some(0.5),
        openai: Some(OpenAILlmParamsConfig {
            reasoning_effort: Some("low".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let body = request_body(set).await;
    assert_eq!(body["reasoning_effort"], "low", "body: {body}");
    assert_eq!(body["max_completion_tokens"], 512, "body: {body}");
    assert_eq!(body["temperature"].as_f64().unwrap(), 0.5, "body: {body}");

    let body = request_body(LlmParamsConfig::default()).await;
    for key in ["reasoning_effort", "max_completion_tokens", "temperature"] {
        assert!(body.get(key).is_none(), "{key} must be absent: {body}");
    }
}
