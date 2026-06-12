use gait_ai::{
    turn::{TurnResult, run_turn},
    session::transcript::read_events,
    session::session_json::read_session_json,
    tools::builtin_tools,
};
use gait_ai::proto::gait::log::v1::{event, payload, Event};

/// The substantive payload kind for a proto event, if it carries one.
fn event_kind(e: &Event) -> Option<&payload::Kind> {
    match &e.body {
        Some(event::Body::Payload(p)) => p.kind.as_ref(),
        _ => None,
    }
}
/// Test emitter: writes transcript, drops the live stream (receiver gone → sends ignored).
fn discard_emitter(gait_dir: &std::path::Path, id: &str) -> gait_ai::Emitter {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<gait_ai::proto::gait::log::v1::StreamItem>();
    gait_ai::Emitter::for_session(gait_dir, id, tx)
}

use gait_ai::provider::{Provider, StreamChunk, ProviderError};
use gait_ai::types::{Message, Tool};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::pin::Pin;
use async_stream::stream;
use tempfile::TempDir;

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
    ) -> Result<Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>, ProviderError> {
        let i = self.idx.fetch_add(1, Ordering::SeqCst);
        let chunks = self.calls.get(i).cloned().unwrap_or_default();
        Ok(Box::pin(stream! { for c in chunks { yield Ok(c); } }))
    }
}

/// Verify that spawn_agent creates a child session with correct assigned_by and from fields.
#[tokio::test]
async fn spawn_agent_via_run_turn_creates_child_session() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    std::fs::write(ws.path().join("gait.jsonc"),
        r#"{"model":"minimax/MiniMax-M3","allow":["spawn_agent"],"tools":[]}"#).unwrap();

    let provider = Arc::new(ScriptedProvider {
        calls: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(),
                    tool_name: "spawn_agent".into(),
                    args: serde_json::json!({ "task": "fix the failing tests" }),
                },
                StreamChunk::Done,
            ],
            vec![
                StreamChunk::Text("Spawned child agent. Done.".into()),
                StreamChunk::Done,
            ],
        ]),
        idx: Arc::new(AtomicUsize::new(0)),
    });

    let tools = builtin_tools(
        gait_ai::tools::ToolCtx::new(ws.path().to_path_buf(), ws.path().to_path_buf(), "sparent001".into()),
        vec![], vec![],
        gait_ai::git::Repos::production(),
    );

    let result = run_turn(
        &gait_ai::session::Session {
            id: "sparent001".into(),
            gait_dir: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            assigned_by: None,
            repos: gait_ai::git::Repos::production(),
        },
        &discard_emitter(ws.path(), "sparent001"),
        gait_ai::profile::ResolvedProfile::for_test(provider, tools, vec!["spawn_agent".into()], vec![]),
        gait_ai::turn::TurnInput { task: Some("coordinate agents".into()), auto: true, max_iter: None },
    ).await.unwrap();

    assert!(matches!(result, TurnResult::Done { .. }));

    // Find the child session by looking for session/* branches
    let repo = git2::Repository::open(ws.path()).unwrap();
    let mut child_id: Option<String> = None;
    for branch in repo.branches(Some(git2::BranchType::Local)).unwrap().flatten() {
        let (b, _) = branch;
        if let Ok(Some(name)) = b.name() {
            if let Some(id) = name.strip_prefix("session/") {
                if id != "sparent001" {
                    child_id = Some(id.to_string());
                }
            }
        }
    }
    let child_id = child_id.expect("child session should have been created");

    // Verify child session.json has assigned_by = sparent001
    let sj = read_session_json(ws.path(), &child_id);
    assert_eq!(sj.assigned_by.as_deref(), Some("sparent001"),
        "child session.json must have assigned_by");

    // Verify child transcript has user_turn with from = agent:sparent001
    let tx = ws.path().join(format!("sessions/{}/transcript.jsonl", child_id));
    let events = read_events(&tx).unwrap();
    let user_turn = events.iter().find_map(|e| match event_kind(e) {
        Some(payload::Kind::UserTurn(u)) => Some(u),
        _ => None,
    }).expect("child transcript must have user_turn");
    assert_eq!(user_turn.content, "fix the failing tests");
    assert_eq!(user_turn.from.as_deref(), Some("agent:sparent001"),
        "child user_turn must have from=agent:sparent001");
}

/// Verify that run_turn returns TurnResult::Permissions when a tool call is blocked in non-auto mode.
#[tokio::test]
async fn run_turn_permissions_result_integration() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(ScriptedProvider {
        calls: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(),
                    tool_name: "shell".into(),
                    args: serde_json::json!({ "cmd": "rm -rf /", "reason": "cleanup" }),
                },
                StreamChunk::Done,
            ],
        ]),
        idx: Arc::new(AtomicUsize::new(0)),
    });

    let result = run_turn(
        &gait_ai::session::Session {
            id: "spermint".into(),
            gait_dir: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            assigned_by: None,
            repos: gait_ai::git::Repos::production(),
        },
        &discard_emitter(ws.path(), "spermint"),
        gait_ai::profile::ResolvedProfile::for_test(provider, vec![], vec![], vec![]),
        gait_ai::turn::TurnInput { task: Some("destroy everything".into()), auto: false, max_iter: None },
    ).await.unwrap();

    assert!(matches!(result, TurnResult::Permissions),
        "expected Permissions result when blocked in non-auto mode");
}
