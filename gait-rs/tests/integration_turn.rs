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
        _t: &[Arc<dyn Tool>],
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
    // create_session_worktree's orphan tree starts empty -- the nested
    // sessions/<id>/ dir doesn't exist until the session's first write, which
    // run_turn's own setup would normally create; since we write directly via
    // emitter.record() before run_turn, we must create it ourselves first.
    std::fs::create_dir_all(
        gait_core::session::live::log_path(ws.path(), "sint001")
            .parent()
            .unwrap(),
    )
    .unwrap();
    for body in gait_core::embed::user_turn_events(&repos, ws.path(), "list the files".into()) {
        emitter.record(body).unwrap();
    }
    let result = run_turn(
        &session,
        &emitter,
        gait_core::profile::ResolvedProfile::for_test(
            provider,
            vec![Arc::new(ls_tool)],
            vec!["ls".into()],
            vec![],
        ),
        gait_core::turn::TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: gait_core::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: Default::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        },
    )
    .await
    .unwrap();
    assert!(matches!(result, gait_core::turn::TurnResult::Done { .. }));
    // Verify transcript
    let tx = gait_core::session::live::log_path(ws.path(), "sint001");
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
                Some(payload::Kind::RunComplete(_)) => "run_complete",
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
    assert!(!event_types.contains(&"run_complete".to_string()));

    // Transcript is valid JSONL
    let raw = std::fs::read_to_string(&tx).unwrap();
    for line in raw.lines().filter(|l| !l.is_empty()) {
        serde_json::from_str::<serde_json::Value>(line).expect("invalid JSON line");
    }
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
            vec![Arc::new(shell_tool)],
            vec!["shell:true".into()],
            vec![],
        ),
        gait_core::turn::TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: gait_core::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: Default::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
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

    let first = run_turn(
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
            sandbox_cfg: Default::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        },
    )
    .await
    .unwrap();
    assert!(matches!(first, gait_core::turn::TurnResult::Done { .. }));

    let second = run_turn(
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
            sandbox_cfg: Default::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        },
    )
    .await
    .unwrap();
    assert!(matches!(second, gait_core::turn::TurnResult::Done { .. }));

    let tx = gait_core::session::live::log_path(ws.path(), "sint002");
    let events = read_events(&tx).unwrap();
    let answer_count = events
        .iter()
        .filter(|e| matches!(event_kind(e), Some(payload::Kind::AssistantText(_))))
        .count();
    assert_eq!(
        answer_count, 2,
        "expected one durable answer per brain turn"
    );
    assert!(!events.iter().any(|event| matches!(
        event_kind(event),
        Some(payload::Kind::TurnResult(_) | payload::Kind::RunComplete(_))
    )));

    let session_start_count = events
        .iter()
        .filter(|e| matches!(event_kind(e), Some(payload::Kind::SessionStart(_))))
        .count();
    assert_eq!(
        session_start_count, 1,
        "session_start should appear exactly once"
    );
}

/// Regression: a plain reply (assistant text with NO tool call) must be streamed
/// as text deltas, not merely recorded as a final `AssistantText` event. The REPL
/// view renders ONLY deltas (AssistantText is a no-op), so when `stream.rs`
/// dropped the pre-tool `publish_delta` (commit c13b565), plain replies silently
/// stopped appearing on screen — "I'm not seeing the reply". This drives a real
/// turn through `run_turn`/`stream::run_step` with a live stream channel and
/// asserts the reply text arrived as a delta.
#[tokio::test]
async fn plain_reply_is_streamed_as_delta() {
    use gait_core::proto::gait::log::v1::stream_item;

    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    const REPLY: &str = "Hello! How can I help you today?";
    let provider = Arc::new(ScriptedProvider {
        // One step, no tool call: just stream the reply text, then finish.
        calls: Arc::new(vec![vec![
            StreamChunk::Text(REPLY.into()),
            StreamChunk::Done,
        ]]),
        idx: Arc::new(AtomicUsize::new(0)),
    });

    let repos = gait_core::git::Repos::production();
    let session = gait_core::session::Session {
        id: "splain01".into(),
        gait_dir: ws.path().to_path_buf(),
        codebase_path: ws.path().to_path_buf(),
        assigned_by: None,
        repos: repos.clone(),
    };

    // Keep the stream receiver ALIVE (unlike `discard_emitter`) so we can observe
    // the deltas the turn publishes to the live channel.
    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<gait_core::proto::gait::log::v1::StreamItem>();
    let emitter = gait_core::Recorder::for_session(ws.path(), "splain01", tx);
    repos
        .traces
        .create_session_worktree(ws.path(), "splain01")
        .unwrap();
    std::fs::create_dir_all(
        gait_core::session::live::log_path(ws.path(), "splain01")
            .parent()
            .unwrap(),
    )
    .unwrap();
    for body in gait_core::embed::user_turn_events(&repos, ws.path(), "hello".into()) {
        emitter.record(body).unwrap();
    }

    run_turn(
        &session,
        &emitter,
        gait_core::profile::ResolvedProfile::for_test(provider, vec![], vec![], vec![]),
        gait_core::turn::TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: gait_core::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: Default::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        },
    )
    .await
    .unwrap();

    let mut streamed = String::new();
    while let Ok(item) = rx.try_recv() {
        if let Some(stream_item::Item::Delta(d)) = item.item {
            streamed.push_str(&d.content);
        }
    }

    assert!(
        streamed.contains(REPLY),
        "plain no-tool reply must be published as a streamed delta so the REPL can \
         render it (regression: stream.rs dropped the pre-tool publish_delta); \
         streamed deltas were: {streamed:?}"
    );
}
