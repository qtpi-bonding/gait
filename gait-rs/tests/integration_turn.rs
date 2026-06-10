use gait_ai::{
    turn::run_turn,
    session::transcript::{read_events, AnyEvent},
};
use gait_ai::provider::{Provider, StreamChunk, ProviderError};
use gait_ai::types::{Message, Tool, ToolDef};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use tempfile::TempDir;
use async_stream::stream;

fn init_git_repo(dir: &std::path::Path) {
    let repo = git2::Repository::init(dir).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
    repo.find_branch("master", git2::BranchType::Local)
        .and_then(|mut b| { b.rename("main", false)?; Ok(()) }).ok();
}

struct ScriptedProvider {
    calls: Arc<Vec<Vec<StreamChunk>>>,
    idx: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn model_id(&self) -> &str { "scripted" }
    async fn stream(
        &self, _s: &str, _m: &[Message], _t: &[Box<dyn Tool>],
    ) -> Result<std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>, ProviderError> {
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
        execute: Box::new(|_| Box::pin(async { Ok(serde_json::Value::String("README.md".into())) })),
        permission: None,
    };

    run_turn(
        &gait_ai::session::Session {
            id: "sint001".into(),
            workspace: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            model: "mock/test".into(),
            assigned_by: None,
            repos: gait_ai::git::Repos::production(),
        },
        gait_ai::profile::ResolvedProfile::for_test(provider, vec![Box::new(ls_tool)], vec!["ls".into()], vec![]),
        gait_ai::turn::TurnInput { task: Some("list the files".into()), auto: true, max_iter: None },
    ).await.unwrap();
    // Verify transcript
    let tx = ws.path().join("sessions/sint001/transcript.jsonl");
    let events = read_events(&tx).unwrap();
    let event_types: Vec<String> = events.iter().map(|e| match e {
        AnyEvent::SessionStart { .. } => "session_start",
        AnyEvent::UserTurn { .. } => "user_turn",
        AnyEvent::ToolCall { .. } => "tool_call",
        AnyEvent::ToolResult { .. } => "tool_result",
        AnyEvent::AssistantText { .. } => "assistant_text",
        AnyEvent::TurnComplete { .. } => "turn_complete",
        _ => "other",
    }.to_string()).collect();

    assert!(event_types.contains(&"session_start".to_string()), "missing session_start: {:?}", event_types);
    assert!(event_types.contains(&"user_turn".to_string()), "missing user_turn: {:?}", event_types);
    assert!(event_types.contains(&"tool_call".to_string()), "missing tool_call: {:?}", event_types);
    assert!(event_types.contains(&"tool_result".to_string()), "missing tool_result: {:?}", event_types);
    assert!(event_types.contains(&"assistant_text".to_string()), "missing assistant_text: {:?}", event_types);
    assert!(event_types.contains(&"turn_complete".to_string()), "missing turn_complete: {:?}", event_types);

    // Transcript is valid JSONL
    let raw = std::fs::read_to_string(&tx).unwrap();
    for line in raw.lines().filter(|l| !l.is_empty()) {
        serde_json::from_str::<serde_json::Value>(line).expect("invalid JSON line");
    }

    // Verify git commit in session branch
    let session_repo = git2::Repository::open(ws.path().join("sessions/sint001")).unwrap();
    let commit_msg = session_repo.head().unwrap().peel_to_commit().unwrap()
        .message().unwrap().to_string();
    assert!(commit_msg.contains("turn 1"), "unexpected commit message: {commit_msg}");
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
        &gait_ai::session::Session {
            id: "sint002".into(),
            workspace: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            model: "mock/test".into(),
            assigned_by: None,
            repos: gait_ai::git::Repos::production(),
        },
        gait_ai::profile::ResolvedProfile::for_test(provider.clone(), vec![], vec![], vec![]),
        gait_ai::turn::TurnInput { task: Some("task 1".into()), auto: true, max_iter: None },
    ).await.unwrap();

    run_turn(
        &gait_ai::session::Session {
            id: "sint002".into(),
            workspace: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            model: "mock/test".into(),
            assigned_by: None,
            repos: gait_ai::git::Repos::production(),
        },
        gait_ai::profile::ResolvedProfile::for_test(provider, vec![], vec![], vec![]),
        gait_ai::turn::TurnInput { task: Some("task 2".into()), auto: true, max_iter: None },
    ).await.unwrap();

    let tx = ws.path().join("sessions/sint002/transcript.jsonl");
    let events = read_events(&tx).unwrap();
    let turn_count = events.iter().filter(|e| matches!(e, AnyEvent::TurnComplete { .. })).count();
    assert_eq!(turn_count, 2, "expected 2 turn_complete events");

    let session_start_count = events.iter().filter(|e| matches!(e, AnyEvent::SessionStart { .. })).count();
    assert_eq!(session_start_count, 1, "session_start should appear exactly once");
}
