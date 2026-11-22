//! A log written by an older gait holds a retired process-update event key.
//! No migration exists: opening such a session fails with a coded, exit-65
//! error naming the log file and the line, and the log is left untouched.
use gait_state::proto::gait::log::v1::{payload, UserText};
use std::path::{Path, PathBuf};
use std::process::Command;

const BAD: &str = "sbad001";
const GOOD: &str = "sgood01";

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(dir: &Path, args: &[&str]) -> Ran {
    let out = Command::new(env!("CARGO_BIN_EXE_gait"))
        .args(args)
        .arg("--gait-dir")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("GAIT_DIR")
        .output()
        .unwrap();
    Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn seed_session(dir: &Path, id: &str) -> PathBuf {
    gait_state::git::Repos::production()
        .traces
        .create_session_worktree(dir, id)
        .unwrap();
    let path = gait_state::session::live::log_path(dir, id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body = gait_state::proto::wire::body(payload::Kind::UserText(UserText {
        content: "hi".into(),
    }));
    gait_state::proto::wire::append(&path, body).unwrap();
    path
}

/// A workspace with one good session and one whose second log line holds the
/// retired key. Returns (dir, bad log path).
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let ws = tempfile::TempDir::new().unwrap();
    let init = Command::new(env!("CARGO_BIN_EXE_gait"))
        .arg("init")
        .arg("--gait-dir")
        .arg(ws.path())
        .output()
        .unwrap();
    assert!(init.status.success());
    seed_session(ws.path(), GOOD);
    let bad = seed_session(ws.path(), BAD);
    let first = std::fs::read_to_string(&bad).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(first.trim()).unwrap();
    value["seq"] = serde_json::json!(2);
    let retired_key = format!("{}Update", ["dae", "mon"].concat());
    value["payload"] = serde_json::json!({ retired_key: { "name": "build" } });
    std::fs::write(&bad, format!("{first}{value}\n")).unwrap();
    (ws, bad)
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.to_string_lossy().contains(".bak") {
                found.push(path);
            }
        }
    }
    found
}

fn assert_retired(ran: &Ran, bad: &Path) {
    let text = format!("{}{}", ran.stdout, ran.stderr);
    assert_eq!(ran.code, Some(65), "{text}");
    assert!(!text.contains("panicked"), "{text}");
    assert!(text.contains("gait.session.retired_event_kind"), "{text}");
    assert!(text.contains(&bad.display().to_string()), "{text}");
    assert!(text.contains("line 2"), "{text}");
    assert!(text.contains("older gait"), "{text}");
}

fn assert_untouched(ws: &Path, bad: &Path, before: &[u8]) {
    assert_eq!(std::fs::read(bad).unwrap(), before);
    assert!(backups(ws).is_empty(), "no .bak may be written");
}

#[test]
fn log_names_the_file_and_line_of_the_retired_kind() {
    let (ws, bad) = workspace();
    let before = std::fs::read(&bad).unwrap();
    assert_retired(&run(ws.path(), &["log", "--id", BAD]), &bad);
    assert_untouched(ws.path(), &bad, &before);
}

#[test]
fn stats_names_the_file_and_line_of_the_retired_kind() {
    let (ws, bad) = workspace();
    let before = std::fs::read(&bad).unwrap();
    assert_retired(&run(ws.path(), &["stats", "--id", BAD]), &bad);
    assert_untouched(ws.path(), &bad, &before);
}

#[test]
fn undo_names_the_file_and_line_of_the_retired_kind() {
    let (ws, bad) = workspace();
    let before = std::fs::read(&bad).unwrap();
    assert_retired(&run(ws.path(), &["undo", "--id", BAD, "--list"]), &bad);
    assert_untouched(ws.path(), &bad, &before);
}

#[test]
fn continuing_the_session_names_the_file_and_line_of_the_retired_kind() {
    let (ws, bad) = workspace();
    let before = std::fs::read(&bad).unwrap();
    assert_retired(&run(ws.path(), &["--session", BAD, "hello"]), &bad);
    assert_untouched(ws.path(), &bad, &before);
}

#[test]
fn sessions_still_lists_the_other_sessions() {
    let (ws, bad) = workspace();
    let before = std::fs::read(&bad).unwrap();
    let ran = run(ws.path(), &["sessions"]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert!(ran.stdout.contains(GOOD), "{}", ran.stdout);
    assert!(!ran.stderr.contains("panicked"), "{}", ran.stderr);
    assert_untouched(ws.path(), &bad, &before);
}
