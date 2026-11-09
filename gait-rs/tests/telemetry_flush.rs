//! The brain flushes telemetry from async code, right before the spine kills it.
//! Two things must hold: the flush really exports what is buffered, and a
//! collector that is down cannot hold the process. Its own test binary, because
//! the telemetry subscriber is global.
#![cfg(feature = "telemetry")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// A stub OTLP collector: answers every request 200 and sends the request line
/// of each over the channel.
fn stub_collector() -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let sender = sender.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let mut head = Vec::new();
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&buf[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request_line = String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let _ = sender.send(request_line);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            });
        }
    });
    (endpoint, receiver)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flush_from_async_code_exports_what_is_buffered() {
    let (endpoint, requests) = stub_collector();
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint);
    gait_core::telemetry::init("gait-flush-test", None, false);
    tracing::info_span!("gait.flush_test").in_scope(|| tracing::info!("buffered"));

    gait_core::telemetry::flush();

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !seen.iter().any(|r: &String| r.contains("/v1/traces")) {
        if let Ok(line) = requests.recv_timeout(Duration::from_millis(200)) {
            seen.push(line);
        }
    }
    assert!(
        seen.iter().any(|line| line.starts_with("POST /v1/traces")),
        "the flush exported no spans; the collector saw {seen:?}"
    );
}
