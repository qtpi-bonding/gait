use gait_core::proto::gait::log::v1::{event, payload, Event};
use gait_core::{session::log::read_events, turn::run_turn};

/// The substantive payload kind for a proto event, if it carries one.
fn event_kind(e: &Event) -> Option<&payload::Kind> {
    match &e.body {
        Some(event::Body::Payload(p)) => p.kind.as_ref(),
        _ => None,
    }
}
use async_stream::stream;
use gait_core::provider::{Provider, ProviderError, StreamChunk};
use gait_core::types::{Message, Tool, ToolDef};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tempfile::TempDir;

/// Test emitter: writes transcript, drops the live stream (receiver gone → sends ignored).
fn discard_emitter(gait_dir: &std::path::Path, id: &str) -> gait_core::Recorder {
    let (tx, _rx) =
        tokio::sync::mpsc::unbounded_channel::<gait_core::proto::gait::log::v1::StreamItem>();
    gait_core::Recorder::for_session(gait_dir, id, tx)
}

fn init_git_repo(dir: &std::path::Path) {
    let repo = git2::Repository::init(dir).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    repo.find_branch("master", git2::BranchType::Local)
        .and_then(|mut b| {
            b.rename("main", false)?;
            Ok(())
        })
        .ok();
}

struct ScriptedProvider {
    calls: Arc<Vec<Vec<StreamChunk>>>,
    idx: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn model_id(&self) -> &str {
        "scripted"
    }
    async fn stream(
        &self,
        _s: &str,
        _m: &[Message],
        _t: &[Box<dyn Tool>],
        _params: &gait_core::config::LlmParamsConfig,
        _tail_context: Option<&str>,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>,
        >,
        ProviderError,
    > {
        let i = self.idx.fetch_add(1, Ordering::SeqCst);
        let chunks = self.calls.get(i).cloned().unwrap_or_default();
        Ok(Box::pin(stream! { for c in chunks { yield Ok(c); } }))
    }
}

#[tokio::test]
async fn full_turn_creates_transcript_and_git_commit() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    std::fs::write(ws.path().join("README.md"), "hello").unwrap();

    let provider = Arc::new(ScriptedProvider {
        calls: Arc::new(vec![
            // Step 1: call ls tool
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "id1".into(),
                    tool_name: "ls".into(),
                    args: serde_json::json!({}),
                },
                StreamChunk::Done,
            ],
            // Step 2: final answer
            vec![
                StreamChunk::Text("I listed the files. Done.".into()),
                StreamChunk::Done,
            ],
        ]),
        idx: Arc::new(AtomicUsize::new(0)),
    });

    let ls_tool = ToolDef {
        name: "ls".into(),
        description: "list files".into(),
        parameters: serde_json::json!({ "type": "object" }),
        execute: Box::new(|_| {
            Box::pin(async { Ok(serde_json::Value::String("README.md".into())) })
        }),
        permission: None,
    };

    let repos = gait_core::git::Repos::production();
    let session = gait_core::session::Session {
        id: "sint001".into(),
        gait_dir: ws.path().to_path_buf(),
        codebase_path: ws.path().to_path_buf(),
        assigned_by: None,
        repos: repos.clone(),
    };
    let emitter = discard_emitter(ws.path(), "sint001");
    // Phase 4 Option A: the caller appends the user_turn, run_turn folds it.
    repos
        .traces
        .create_session_worktree(ws.path(), "sint001")
        .unwrap();
    for body in gait_core::embed::user_turn_events(&repos, ws.path(), "list the files".into()) {
        emitter.record(body).unwrap();
    }
    run_turn(
        &session,
        &emitter,
        gait_core::profile::ResolvedProfile::for_test(
            provider,
            vec![Box::new(ls_tool)],
            vec!["ls".into()],
            vec![],
        ),
        gait_core::turn::TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: gait_core::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
        },
    )
    .await
    .unwrap();
    // Verify transcript
    let tx = ws.path().join("sessions/sint001/log.jsonl");
    let events = read_events(&tx).unwrap();
    let event_types: Vec<String> = events
        .iter()
        .map(|e| {
            match event_kind(e) {
                Some(payload::Kind::SessionStart(_)) => "session_start",
                Some(payload::Kind::UserText(_)) => "user_turn",
                Some(payload::Kind::ToolCall(_)) => "tool_call",
                Some(payload::Kind::ToolResult(_)) => "tool_result",
                Some(payload::Kind::AssistantText(_)) => "assistant_text",
                Some(payload::Kind::TurnComplete(_)) => "turn_complete",
                _ => "other",
            }
            .to_string()
        })
        .collect();

    assert!(
        event_types.contains(&"session_start".to_string()),
        "missing session_start: {:?}",
        event_types
    );
    assert!(
        event_types.contains(&"user_turn".to_string()),
        "missing user_turn: {:?}",
        event_types
    );
    assert!(
        event_types.contains(&"tool_call".to_string()),
        "missing tool_call: {:?}",
        event_types
    );
    assert!(
        event_types.contains(&"tool_result".to_string()),
        "missing tool_result: {:?}",
        event_types
    );
    assert!(
        event_types.contains(&"assistant_text".to_string()),
        "missing assistant_text: {:?}",
        event_types
    );
    assert!(
        event_types.contains(&"turn_complete".to_string()),
        "missing turn_complete: {:?}",
        event_types
    );

    // Transcript is valid JSONL
    let raw = std::fs::read_to_string(&tx).unwrap();
    for line in raw.lines().filter(|l| !l.is_empty()) {
        serde_json::from_str::<serde_json::Value>(line).expect("invalid JSON line");
    }

    // Verify git commit in session branch
    let session_repo = git2::Repository::open(ws.path().join("sessions/sint001")).unwrap();
    let commit_msg = session_repo
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .message()
        .unwrap()
        .to_string();
    assert!(
        commit_msg.contains("turn 1"),
        "unexpected commit message: {commit_msg}"
    );
}

/// Phase 2 end-to-end: a `cwd`-emitting tool call inside a real `run_turn` must
/// reach the LIVE channel (not only the transcript) — proving the turn threads
/// its `Recorder` into `Tool::execute`, so `StdioSink`/REPL see it on stderr.
#[tokio::test]
async fn cwd_event_reaches_live_channel_through_turn() {
    use gait_core::proto::gait::log::v1::{event, stream_item, StreamItem};
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(ScriptedProvider {
        calls: Arc::new(vec![
            // Step 1: shell `true` with cwd "." → emits a Cwd event through the Recorder.
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "id1".into(),
                    tool_name: "shell".into(),
                    args: serde_json::json!({ "cmd": "true", "reason": "t", "cwd": "." }),
                },
                StreamChunk::Done,
            ],
            // Step 2: final answer.
            vec![StreamChunk::Text("done".into()), StreamChunk::Done],
        ]),
        idx: Arc::new(AtomicUsize::new(0)),
    });

    let shell_tool = gait_core::tools::ShellTool::new(gait_core::tools::ToolCtx::new(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "sintcwd".into(),
    ));

    // Keep the receiver: we assert the Cwd StreamItem was PUBLISHED, not just logged.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();
    let rec = gait_core::Recorder::for_session(ws.path(), "sintcwd", tx);

    run_turn(
        &gait_core::session::Session {
            id: "sintcwd".into(),
            gait_dir: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            assigned_by: None,
            repos: gait_core::git::Repos::production(),
        },
        &rec,
        gait_core::profile::ResolvedProfile::for_test(
            provider,
            vec![Box::new(shell_tool)],
            vec!["shell:true".into()],
            vec![],
        ),
        gait_core::turn::TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: gait_core::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
        },
    )
    .await
    .unwrap();

    drop(rec); // close the channel so the drain terminates
    let mut saw_cwd = false;
    while let Some(item) = rx.recv().await {
        if let Some(stream_item::Item::Event(ev)) = item.item {
            if let Some(event::Body::Payload(p)) = ev.body {
                if matches!(p.kind, Some(payload::Kind::Cwd(_))) {
                    saw_cwd = true;
                }
            }
        }
    }
    assert!(
        saw_cwd,
        "cwd event must reach the live channel through a full turn"
    );
}

#[tokio::test]
async fn second_turn_resumes_session() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(ScriptedProvider {
        calls: Arc::new(vec![
            vec![StreamChunk::Text("Turn 1 done.".into()), StreamChunk::Done],
            vec![StreamChunk::Text("Turn 2 done.".into()), StreamChunk::Done],
        ]),
        idx: Arc::new(AtomicUsize::new(0)),
    });

    run_turn(
        &gait_core::session::Session {
            id: "sint002".into(),
            gait_dir: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            assigned_by: None,
            repos: gait_core::git::Repos::production(),
        },
        &discard_emitter(ws.path(), "sint002"),
        gait_core::profile::ResolvedProfile::for_test(provider.clone(), vec![], vec![], vec![]),
        gait_core::turn::TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: gait_core::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
        },
    )
    .await
    .unwrap();

    run_turn(
        &gait_core::session::Session {
            id: "sint002".into(),
            gait_dir: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            assigned_by: None,
            repos: gait_core::git::Repos::production(),
        },
        &discard_emitter(ws.path(), "sint002"),
        gait_core::profile::ResolvedProfile::for_test(provider, vec![], vec![], vec![]),
        gait_core::turn::TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: gait_core::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
        },
    )
    .await
    .unwrap();

    let tx = ws.path().join("sessions/sint002/log.jsonl");
    let events = read_events(&tx).unwrap();
    let turn_count = events
        .iter()
        .filter(|e| matches!(event_kind(e), Some(payload::Kind::TurnComplete(_))))
        .count();
    assert_eq!(turn_count, 2, "expected 2 turn_complete events");

    let session_start_count = events
        .iter()
        .filter(|e| matches!(event_kind(e), Some(payload::Kind::SessionStart(_))))
        .count();
    assert_eq!(
        session_start_count, 1,
        "session_start should appear exactly once"
    );
}
