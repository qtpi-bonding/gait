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

#[tokio::test]
async fn stream_ends_cleanly_when_session_is_persisted() {
    let tmp = tempfile::tempdir().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    let dir = session_dirs(&gait_dir, "s1");
    let log_path = dir.join("log.jsonl");
    append_line(&log_path, &event_line("e1"));

    let hub = SessionWatchHub::new(gait_dir.clone()).unwrap();
    let mut stream = watch_session_log(&hub, &gait_dir, "s1").await.unwrap();

    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(event_marker(&first), Some("e1".to_string()));

    // Simulate persist: delete .active/s1/, materialize sessions/s1/log.jsonl.
    std::fs::remove_dir_all(gait_dir.join(".active").join("s1")).unwrap();
    let persisted_dir = gait_dir.join("sessions").join("s1");
    std::fs::create_dir_all(&persisted_dir).unwrap();
    std::fs::write(
        persisted_dir.join("log.jsonl"),
        format!("{}\n", event_line("e1")),
    )
    .unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("stream should end, not hang");
    assert!(
        outcome.is_none(),
        "stream must end cleanly (None), not error, after persist"
    );
}

#[tokio::test]
async fn stream_ends_cleanly_when_session_is_discarded() {
    let tmp = tempfile::tempdir().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    let dir = session_dirs(&gait_dir, "s1");
    let log_path = dir.join("log.jsonl");
    append_line(&log_path, &event_line("e1"));

    let hub = SessionWatchHub::new(gait_dir.clone()).unwrap();
    let mut stream = watch_session_log(&hub, &gait_dir, "s1").await.unwrap();

    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(event_marker(&first), Some("e1".to_string()));

    // Simulate discard: delete .active/s1/, nothing replaces it.
    std::fs::remove_dir_all(gait_dir.join(".active").join("s1")).unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("stream should end, not hang");
    assert!(
        outcome.is_none(),
        "stream must end cleanly (None), not error, after discard"
    );
}

#[tokio::test]
async fn watch_attached_before_first_write_waits_instead_of_ending_early() {
    // Regression test: a watch can attach before the session directory (or
    // its log file) exists at all. The tail loop must wait for it to appear.
    let tmp = tempfile::tempdir().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    // Deliberately leave session s1 absent when the watch attaches.
    std::fs::create_dir_all(gait_dir.join(".active")).unwrap();

    let hub = SessionWatchHub::new(gait_dir.clone()).unwrap();
    let mut stream = watch_session_log(&hub, &gait_dir, "s1").await.unwrap();

    let premature = tokio::time::timeout(Duration::from_millis(450), stream.next()).await;
    assert!(
        premature.is_err(),
        "stream must not end before the log file has ever existed"
    );

    let dir = session_dirs(&gait_dir, "s1");
    append_line(&dir.join("log.jsonl"), &event_line("e1"));

    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("timed out waiting for the event once the file appeared")
        .unwrap()
        .unwrap();
    assert_eq!(event_marker(&first), Some("e1".to_string()));
}
