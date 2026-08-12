//! Phase 2 integration tests for session/prompt, driven over the SDK's
//! in-process channel (same harness as tests/skeleton.rs).
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    InitializeRequest, NewSessionRequest, PromptRequest, SessionId, SessionNotification,
    SessionUpdate, StopReason,
};
use agent_client_protocol::{Client, ConnectTo};
use gait_acp::session_map::SessionMap;

use prost::Message;
use std::path::Path;

#[tokio::test]
async fn prompt_unknown_session_errors_not_panics() {
    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);

    let (prompt_result, init_ok) = Client
        .connect_with(channel, async |cx| {
            let prompt_result = cx
                .send_request(PromptRequest::new(
                    SessionId::new("never-minted"),
                    vec!["hi".to_string().into()],
                ))
                .block_task()
                .await;
            // Connection must survive a bad prompt.
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            Ok((prompt_result, init))
        })
        .await
        .expect("connection stays alive");

    assert!(
        prompt_result.is_err(),
        "unknown session id -> JSON-RPC error"
    );
    assert!(init_ok.auth_methods.is_empty());
}

/// `[1u8][len u32 BE][encoded Signal]` — the Nerve frame the spine reads
/// (mirrors gait-core's tests/common::frame).
fn frame(sig: &gait_proto::gait::sandbox::v1::Signal) -> Vec<u8> {
    let b = sig.encode_to_vec();
    let mut f = vec![1u8];
    f.extend_from_slice(&(b.len() as u32).to_be_bytes());
    f.extend_from_slice(&b);
    f
}

/// Bake mock_frames.bin: one settled AssistantText event, then Terminated(done).
fn bake_say_hi(gait_dir: &Path) {
    use gait_core::proto::gait::log::v1::{StreamItem, stream_item};
    use gait_proto::gait::sandbox::v1::{Append, Signal, StreamForward, Terminated, signal};

    let assistant = gait_core::proto::envelope::mint(
        gait_core::proto::events::assistant_text("hi there".into()),
        None,
        gait_core::proto::envelope::EventScope::Step {
            run: 0,
            turn: 0,
            step: 0,
        },
    );
    let append_sig = Signal {
        corr_id: 0,
        body: Some(signal::Body::Append(Append {
            event: Some(assistant.clone()),
        })),
    };
    let event_sig = Signal {
        corr_id: 0,
        body: Some(signal::Body::StreamForward(StreamForward {
            item: Some(StreamItem {
                item: Some(stream_item::Item::Event(assistant)),
            }),
        })),
    };
    let done = Signal {
        corr_id: 0,
        body: Some(signal::Body::Terminated(Terminated {
            turn: Some(gait_core::proto::gait::sandbox::v1::FinalizedTurn {
                outcome: gait_core::proto::gait::sandbox::v1::FinalizedTurnOutcome::TextDone as i32,
                message: "ok".into(),
                ..Default::default()
            }),
            failed: false,
        })),
    };
    let mut blob = Vec::new();
    blob.extend_from_slice(&frame(&append_sig));
    blob.extend_from_slice(&frame(&event_sig));
    blob.extend_from_slice(&frame(&done));
    std::fs::write(gait_dir.join("mock_frames.bin"), blob).unwrap();
}

/// Bake mock_frames.bin with only a settled AssistantText event -- NO
/// Terminated -- plus `mock_delay_ms` so the mock brain sleeps `delay_ms`
/// BEFORE emitting anything. The spine stays parked waiting on the brain's
/// stdout the whole time, so the turn (and the `SessionLock` it holds via
/// `Persistence::init_workspace`) is still in flight for at least `delay_ms`.
/// Baking a `Terminated` here would finalize the turn and release the lock
/// the instant the frame is read -- before any delay could matter.
fn bake_hanging(gait_dir: &Path, delay_ms: u64) {
    use gait_core::proto::gait::log::v1::{Event, StreamItem, stream_item};
    use gait_proto::gait::sandbox::v1::{Signal, StreamForward, signal};

    let assistant = Event {
        body: Some(gait_core::proto::events::assistant_text("hi there".into())),
        ..Default::default()
    };
    let event_sig = Signal {
        corr_id: 0,
        body: Some(signal::Body::StreamForward(StreamForward {
            item: Some(StreamItem {
                item: Some(stream_item::Item::Event(assistant)),
            }),
        })),
    };
    std::fs::write(gait_dir.join("mock_frames.bin"), frame(&event_sig)).unwrap();
    std::fs::write(gait_dir.join("mock_delay_ms"), delay_ms.to_string()).unwrap();
}

/// The real happened-before primitive (Task 6 hazard b): poll the session's
/// worktree for its `lock` file instead of guessing with a sleep. Presence of
/// the file means `SessionLock::acquire` has run for this session -- the turn
/// is genuinely in flight, not "probably started by now."
async fn wait_until_locked(gait_dir: &Path, session_id: &str) {
    let lock = gait_dir.join(".active").join(session_id).join("lock");
    for _ in 0..500 {
        if lock.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for session lock at {}", lock.display());
}

/// `SessionLock`'s `Drop` removes the lock file unconditionally, so "the file
/// is gone" is equivalent to "the guard that `Persistence::init_workspace`
/// returned has been dropped" -- i.e. the turn (and the thread that owned it)
/// is done. Checked as a plain file-existence probe rather than a
/// pid-liveness one: the PID written into the lock is this test process's own
/// (the turn runs on an in-process OS thread, not a real brain subprocess), so
/// it is trivially "alive" the whole time regardless of what happens to the
/// mock brain child process.
fn session_is_locked(gait_dir: &Path, session_id: &str) -> bool {
    gait_dir
        .join(".active")
        .join(session_id)
        .join("lock")
        .exists()
}

/// git init + an initial empty-tree commit. Replaces tests/repl_live_render.rs's
/// git2-based helper because gait-acp's tests don't depend on git2 directly; we
/// shell out to the system `git` instead. Same observable effect: a repo with a
/// single commit on the default branch, ready for gait-state's git checks.
fn init_git_repo(dir: &std::path::Path) {
    use std::process::Command;
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    run(&["init"]);
    run(&["commit", "--allow-empty", "-m", "init"]);
    // Rename master -> main when git created a default 'master' (older defaults).
    let _ = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["branch", "-m", "master", "main"])
        .output();
}

#[tokio::test]
async fn prompt_streams_agent_message_and_ends_turn() {
    // A gait-init'd workspace (.gaitref -> out-of-tree gait-dir), git-initialized
    // codebase, mock brain wired via GAIT_BRAIN_EXE.
    set_brain_exe(); // sets GAIT_BRAIN_EXE once — see helper below
    let t = tempfile::TempDir::new().unwrap();
    let gait_dir = t.path().join("home");
    std::fs::create_dir_all(&gait_dir).unwrap();
    // The gait-dir is itself a git repo (GitTraceRepo::create_session_worktree
    // opens it with Repository::open and manages session branches/worktrees
    // inside it) — `gait init` does this for real workspaces; the test must
    // replicate it since nothing else in this harness runs `gait init`.
    init_git_repo(&gait_dir);
    let repo = t.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo); // helper copied from tests/repl_live_render.rs (git2 init + empty commit)
    std::fs::write(
        repo.join(".gaitref"),
        format!("gaitdir: {}\n", gait_dir.display()),
    )
    .unwrap();
    // Resolve a model so ConfigResolver::resolve_checked passes; mock brain ignores it.
    std::fs::create_dir_all(gait_dir.join("inferences/mock")).unwrap();
    std::fs::write(
        gait_dir.join("inferences/mock/INFERENCE.yaml"),
        "model: mock/model\ncontext_window_tokens: 128000\n",
    )
    .unwrap();
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "mock" }"#).unwrap();
    bake_say_hi(&gait_dir);

    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);

    // The client must register an `on_receive_notification` handler to observe
    // agent->client `session/update`s; a bare `Client.connect_with` uses the
    // NullHandler and drops them (verified against the SDK). Push each update
    // into a shared vec.
    let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::<SessionUpdate>::new()));
    let updates_h = updates.clone();
    let stop_reason = Client
        .builder()
        .on_receive_notification(
            move |notif: SessionNotification, _cx| {
                let updates_h = updates_h.clone();
                async move {
                    updates_h.lock().unwrap().push(notif.update);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            let resp = cx
                .send_request(PromptRequest::new(sid, vec!["say hi".to_string().into()]))
                .block_task()
                .await?;
            Ok(resp.stop_reason)
        })
        .await
        .expect("prompt succeeds");

    assert_eq!(stop_reason, StopReason::EndTurn);
    let updates = updates.lock().unwrap();
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::AgentMessageChunk(_))),
        "expected at least one AgentMessageChunk, got {updates:?}"
    );
}

/// Set GAIT_BRAIN_EXE once for the whole test binary (cargo runs a crate's
/// integration tests multithreaded in one process; every test wants the same
/// path, so guard the process-global set behind a Once).
fn set_brain_exe() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    // SAFETY: in tests, the env-var write is a one-shot startup-time hint read by
    // every spawned `gait __brain` subprocess; no other thread reads it during
    // this race window (the Once guarantees serial init).
    ONCE.call_once(|| unsafe {
        std::env::set_var("GAIT_BRAIN_EXE", env!("CARGO_BIN_EXE_mock_brain_acp"));
    });
}

/// A gait-init'd workspace under `root/<name>-home` (`.gaitref` pointing at it
/// from `root/<name>-repo`), both git-initialized, with a minimal `gait.jsonc`
/// so `ConfigResolver::resolve_checked` passes (the mock brain ignores the
/// model). Returns `(gait_dir, codebase)`. Shared setup for the concurrency /
/// lock / drain tests below; `prompt_streams_agent_message_and_ends_turn`
/// above predates this helper and keeps its setup inline.
fn make_workspace(root: &Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let gait_dir = root.join(format!("{name}-home"));
    std::fs::create_dir_all(&gait_dir).unwrap();
    init_git_repo(&gait_dir);
    let repo = root.join(format!("{name}-repo"));
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    std::fs::write(
        repo.join(".gaitref"),
        format!("gaitdir: {}\n", gait_dir.display()),
    )
    .unwrap();
    std::fs::create_dir_all(gait_dir.join("inferences/mock")).unwrap();
    std::fs::write(
        gait_dir.join("inferences/mock/INFERENCE.yaml"),
        "model: mock/model\ncontext_window_tokens: 128000\n",
    )
    .unwrap();
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "mock" }"#).unwrap();
    (gait_dir, repo)
}

#[tokio::test]
async fn two_sessions_make_independent_progress() {
    set_brain_exe();
    let t = tempfile::TempDir::new().unwrap();
    let (gait_dir_a, repo_a) = make_workspace(t.path(), "a");
    let (gait_dir_b, repo_b) = make_workspace(t.path(), "b");
    bake_say_hi(&gait_dir_a);
    bake_say_hi(&gait_dir_b);

    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);

    let (resp_a, resp_b) = Client
        .connect_with(channel, async |cx| {
            let sid_a = cx
                .send_request(NewSessionRequest::new(repo_a.clone()))
                .block_task()
                .await?
                .session_id;
            let sid_b = cx
                .send_request(NewSessionRequest::new(repo_b.clone()))
                .block_task()
                .await?
                .session_id;
            // Fire both prompts before awaiting either -- `send_request`
            // dispatches synchronously, `block_task` only awaits the reply,
            // so this genuinely runs both turns concurrently rather than
            // serializing them.
            let fut_a = cx
                .send_request(PromptRequest::new(sid_a, vec!["say hi".to_string().into()]))
                .block_task();
            let fut_b = cx
                .send_request(PromptRequest::new(sid_b, vec!["say hi".to_string().into()]))
                .block_task();
            let (a, b) = tokio::join!(fut_a, fut_b);
            Ok((a?, b?))
        })
        .await
        .expect("both prompts succeed");

    assert_eq!(resp_a.stop_reason, StopReason::EndTurn);
    assert_eq!(resp_b.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn same_session_second_prompt_errors_on_lock() {
    set_brain_exe();
    let t = tempfile::TempDir::new().unwrap();
    let (gait_dir, repo) = make_workspace(t.path(), "lockcheck");
    // Prompt #1's brain sleeps 300ms before emitting anything and never sends
    // Terminated, so the spine stays parked with the SessionLock held for at
    // least that long (Task 6 hazard a).
    bake_hanging(&gait_dir, 300);

    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);

    let gait_dir_check = gait_dir.clone();
    let second_result = Client
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            // Fire #1 but do not await it -- its brain is asleep. Bound to a
            // variable so it isn't dropped (and auto-cancelled) until after
            // we've made our assertion.
            let first = cx.send_request(PromptRequest::new(
                sid.clone(),
                vec!["say hi".to_string().into()],
            ));
            // Real happened-before (Task 6 hazard b): wait until #1 has
            // actually acquired the SessionLock, not a wall-clock guess.
            wait_until_locked(&gait_dir_check, &sid.0).await;
            let second = cx
                .send_request(PromptRequest::new(
                    sid,
                    vec!["say hi again".to_string().into()],
                ))
                .block_task()
                .await;
            drop(first);
            Ok(second)
        })
        .await
        .expect("connection stays alive");

    assert!(
        second_result.is_err(),
        "second prompt on a locked session must be a JSON-RPC error, not queue or panic"
    );
}

#[tokio::test]
async fn drain_reaps_in_flight_turn_on_teardown() {
    set_brain_exe();
    let t = tempfile::TempDir::new().unwrap();
    let (gait_dir, repo) = make_workspace(t.path(), "drain");
    // Hang the brain BEFORE it emits Terminated so the SessionLock stays held
    // long enough for `drain()` below to observe it live.
    bake_hanging(&gait_dir, 10_000);

    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map.clone()).into_channel_and_future();
    let server = tokio::spawn(server);

    let gait_dir_check = gait_dir.clone();
    let sid = Client
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            // Fire the prompt and don't await it (the brain is hanging).
            let _ = cx.send_request(PromptRequest::new(
                sid.clone(),
                vec!["hi".to_string().into()],
            ));
            wait_until_locked(&gait_dir_check, &sid.0).await;
            Ok(sid.0.to_string())
        })
        .await
        .expect("session opens");

    // Simulate connection teardown.
    server.abort();
    map.drain();

    // The brain's thread was reaped (SIGTERM'd + joined): the session lock is
    // released.
    assert!(
        !session_is_locked(&gait_dir, &sid),
        "drain must release the turn's SessionLock"
    );
}
