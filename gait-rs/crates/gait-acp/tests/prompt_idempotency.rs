//! End-to-end proof that a repeated `gait/idempotency_key` on `_meta` is
//! rejected instead of re-running the turn — the regression test for the
//! "connection drops after a prompt is sent, caller resends not knowing
//! whether the first attempt landed" case.
use agent_client_protocol::schema::v1::{NewSessionRequest, PromptRequest};
use agent_client_protocol::{Client, ConnectTo};
use gait_acp::session_map::SessionMap;
use std::path::Path;

/// `[1u8][len u32 BE][encoded Signal]` — the Nerve frame the spine reads
/// (mirrors gait-core's tests/common::frame and gait-acp/tests/prompt.rs's
/// copy of the same helper).
fn frame(sig: &gait_proto::gait::sandbox::v1::Signal) -> Vec<u8> {
    use prost::Message;
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
    // Numbered fixtures let the mock brain rebase the baked event scope for
    // each successive turn in this multi-turn test.
    std::fs::write(gait_dir.join("mock_frames.bin"), &blob).unwrap();
    std::fs::write(gait_dir.join("mock_frames.0.bin"), &blob).unwrap();
    std::fs::write(gait_dir.join("mock_frames.1.bin"), blob).unwrap();
}

/// Bake mock_frames.bin with only a settled AssistantText event — NO
/// Terminated — plus `mock_delay_ms` so the mock brain sleeps `delay_ms`
/// before emitting anything. The spine stays parked waiting on the brain's
/// stdout the whole time, so the turn (and the `SessionLock` it holds) is
/// still in flight for at least `delay_ms`. Mirrors prompt.rs's helper of
/// the same name.
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

/// Poll for the session's on-disk lock file instead of guessing with a
/// sleep. Presence means `SessionLock::acquire` has run for this session —
/// the turn is genuinely in flight. Mirrors prompt.rs's helper of the same
/// name.
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

fn init_git_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init"]);
    run(&["commit", "--allow-empty", "-m", "init"]);
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["branch", "-m", "master", "main"])
        .output();
}

fn set_brain_exe() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    // SAFETY: in tests, the env-var write is a one-shot startup-time hint read
    // by every spawned `gait __brain` subprocess; no other thread reads it
    // during this race window (the Once guarantees serial init).
    ONCE.call_once(|| unsafe {
        std::env::set_var("GAIT_BRAIN_EXE", env!("CARGO_BIN_EXE_mock_brain_acp"));
    });
}

fn make_workspace(root: &Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let gait_dir = root.join(format!("{name}-home"));
    std::fs::create_dir_all(&gait_dir).unwrap();
    unsafe { std::env::set_var("GAIT_HOME", &gait_dir) };
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
        "model: mock/model\ncompaction_threshold_tokens: 128000\n",
    )
    .unwrap();
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "mock" }"#).unwrap();
    (gait_dir, repo)
}

#[tokio::test]
async fn repeated_idempotency_key_is_rejected_not_rerun() {
    set_brain_exe();
    let t = tempfile::TempDir::new().unwrap();
    let (gait_dir, repo) = make_workspace(t.path(), "idem");
    bake_say_hi(&gait_dir);

    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);

    let (first_ok, second_err) = Client
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;

            let mut meta = serde_json::Map::new();
            meta.insert(
                "gait/idempotency_key".to_string(),
                serde_json::json!("delivery-1"),
            );

            let first = cx
                .send_request(
                    PromptRequest::new(sid.clone(), vec!["say hi".to_string().into()])
                        .meta(Some(meta.clone())),
                )
                .block_task()
                .await;

            // Second send with the SAME idempotency key, after the first has
            // fully completed (mock brain's Terminated frame already landed) —
            // this is the "connection dropped, caller resends unsure whether
            // the first landed" case, in its worst form: the original turn
            // isn't even in flight any more, so without dedup this would just
            // run a second, real turn.
            let second = cx
                .send_request(
                    PromptRequest::new(sid, vec!["say hi".to_string().into()]).meta(Some(meta)),
                )
                .block_task()
                .await;

            Ok((first, second))
        })
        .await
        .expect("connection stays alive");

    assert!(first_ok.is_ok(), "first prompt with a fresh key succeeds");
    let err = second_err.expect_err("repeated idempotency key must be rejected");
    assert_eq!(
        i32::from(err.code),
        -32001,
        "must use the documented already-seen error code"
    );
    let data = err.data.expect("error must carry data");
    assert_eq!(data["reason"], "already_seen_by_agent");
}

#[tokio::test]
async fn distinct_idempotency_keys_both_succeed() {
    set_brain_exe();
    let t = tempfile::TempDir::new().unwrap();
    let (gait_dir, repo) = make_workspace(t.path(), "idem2");
    bake_say_hi(&gait_dir);

    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);

    let (first, second) = Client
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;

            let mut meta_a = serde_json::Map::new();
            meta_a.insert("gait/idempotency_key".to_string(), serde_json::json!("a"));
            let first = cx
                .send_request(
                    PromptRequest::new(sid.clone(), vec!["say hi".to_string().into()])
                        .meta(Some(meta_a)),
                )
                .block_task()
                .await;

            let mut meta_b = serde_json::Map::new();
            meta_b.insert("gait/idempotency_key".to_string(), serde_json::json!("b"));
            let second = cx
                .send_request(
                    PromptRequest::new(sid, vec!["say hi again".to_string().into()])
                        .meta(Some(meta_b)),
                )
                .block_task()
                .await;

            Ok((first, second))
        })
        .await
        .expect("connection stays alive");

    assert!(first.is_ok(), "first prompt succeeds");
    assert!(
        second.is_ok(),
        "a different idempotency key is a genuinely new prompt, not a dup: {second:?}"
    );
}

/// The admission-order regression: a keyed prompt rejected only because
/// ANOTHER turn is concurrently in flight must not get the already-seen
/// treatment, and — critically — must not be recorded at all, since its
/// prompt never actually ran. `SessionMap` is `Clone` over an inner `Arc`,
/// so the `map` handle kept here observes the exact same state the running
/// server mutated.
#[tokio::test]
async fn key_rejected_by_a_concurrent_turn_is_not_recorded() {
    set_brain_exe();
    let t = tempfile::TempDir::new().unwrap();
    let (gait_dir, repo) = make_workspace(t.path(), "idem3");
    bake_hanging(&gait_dir, 300);

    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map.clone()).into_channel_and_future();
    tokio::spawn(server);

    let gait_dir_check = gait_dir.clone();
    let sid_str = Client
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;

            let mut meta = serde_json::Map::new();
            meta.insert("gait/idempotency_key".to_string(), serde_json::json!("k1"));

            // Fire #1 (no key) but don't await it — its brain is asleep.
            let first = cx.send_request(PromptRequest::new(
                sid.clone(),
                vec!["say hi".to_string().into()],
            ));
            wait_until_locked(&gait_dir_check, &sid.0).await;

            // #2, keyed, arrives while #1 is genuinely in flight.
            let concurrent_result = cx
                .send_request(
                    PromptRequest::new(sid.clone(), vec!["say hi too".to_string().into()])
                        .meta(Some(meta)),
                )
                .block_task()
                .await;
            drop(first);

            let err = concurrent_result.expect_err("concurrent keyed prompt must be rejected");
            let reason = err.data.as_ref().and_then(|d| d.get("reason")).cloned();
            assert_ne!(
                reason,
                Some(serde_json::json!("already_seen_by_agent")),
                "rejection while a turn is genuinely in flight must be the \
                 generic in-flight error, not already-seen — got {err:?}"
            );

            Ok(sid.0.to_string())
        })
        .await
        .expect("connection stays alive");

    assert!(
        !map.has_seen_idempotency_key(&sid_str, "k1"),
        "a key whose prompt was rejected by a concurrent in-flight turn \
         (and so never ran) must not be recorded as seen"
    );
}
