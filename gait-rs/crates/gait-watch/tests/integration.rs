use std::io::Write;
use std::time::Duration;

use futures_util::StreamExt;
use gait_state::gait::log::v1::{payload, Event};
use gait_watch::hub::SessionWatchHub;
use gait_watch::tail::watch_session_log;

fn session_dirs(gait_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    let dir = gait_dir
        .join(".active")
        .join(session_id)
        .join("sessions")
        .join(session_id);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn event_line(marker: &str) -> String {
    let body = gait_state::proto::events::session_start(
        "s1".to_string(),
        None,
        Some(marker.to_string()),
        false,
    );
    let event = gait_state::envelope::mint(body, None, gait_state::envelope::EventScope::Session);
    serde_json::to_string(&event).unwrap()
}

fn event_marker(event: &Event) -> Option<String> {
    match gait_state::proto::event_kind(event) {
        Some(payload::Kind::SessionStart(s)) => s.codebase_path.clone(),
        _ => None,
    }
}

fn append_line(path: &std::path::Path, line: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

#[tokio::test]
async fn backlog_is_emitted_before_live_events() {
    let tmp = tempfile::tempdir().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    let dir = session_dirs(&gait_dir, "s1");
    let log_path = dir.join("log.jsonl");
    append_line(&log_path, &event_line("e1"));
    append_line(&log_path, &event_line("e2"));

    let hub = SessionWatchHub::new(gait_dir.clone()).unwrap();
    let mut stream = watch_session_log(&hub, &gait_dir, "s1").await.unwrap();

    let first = stream.next().await.unwrap().unwrap();
    let second = stream.next().await.unwrap().unwrap();
    assert_eq!(event_marker(&first), Some("e1".to_string()));
    assert_eq!(event_marker(&second), Some("e2".to_string()));
}

#[tokio::test]
async fn live_appends_are_streamed_after_backlog() {
    let tmp = tempfile::tempdir().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    let dir = session_dirs(&gait_dir, "s1");
    let log_path = dir.join("log.jsonl");
    append_line(&log_path, &event_line("e1"));

    let hub = SessionWatchHub::new(gait_dir.clone()).unwrap();
    let mut stream = watch_session_log(&hub, &gait_dir, "s1").await.unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(event_marker(&first), Some("e1".to_string()));
    append_line(&log_path, &event_line("e2"));
    let second = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("timed out waiting for live event")
        .unwrap()
        .unwrap();
    assert_eq!(event_marker(&second), Some("e2".to_string()));
}

#[tokio::test]
async fn mid_write_tick_does_not_emit_partial_event() {
    let tmp = tempfile::tempdir().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    let dir = session_dirs(&gait_dir, "s1");
    let log_path = dir.join("log.jsonl");
    let hub = SessionWatchHub::new(gait_dir.clone()).unwrap();
    let mut stream = watch_session_log(&hub, &gait_dir, "s1").await.unwrap();
    let partial = event_line("e1");
    std::fs::write(&log_path, &partial).unwrap();
    let premature = tokio::time::timeout(Duration::from_millis(450), stream.next()).await;
    assert!(
        premature.is_err(),
        "a non-newline-terminated line must not be emitted yet"
    );
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .unwrap();
    writeln!(f).unwrap();
    let complete = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("timed out waiting for the now-complete event")
        .unwrap()
        .unwrap();
    assert_eq!(event_marker(&complete), Some("e1".to_string()));
}

#[tokio::test]
async fn backlog_larger_than_a_bounded_channel_would_hold_does_not_deadlock() {
    let tmp = tempfile::tempdir().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    let dir = session_dirs(&gait_dir, "s1");
    let log_path = dir.join("log.jsonl");
    for i in 0..200 {
        append_line(&log_path, &event_line(&format!("e{i}")));
    }
    let hub = SessionWatchHub::new(gait_dir.clone()).unwrap();
    let stream = tokio::time::timeout(
        Duration::from_secs(5),
        watch_session_log(&hub, &gait_dir, "s1"),
    )
    .await
    .expect("watch_session_log must not deadlock sending a large backlog")
    .unwrap();
    let events: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.take(200).collect())
        .await
        .expect("draining the backlog must not hang");
    assert_eq!(events.len(), 200);
}
