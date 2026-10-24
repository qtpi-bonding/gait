use gait_core::proto::gait::log::v1::{event, payload, Event};
use gait_core::{
    session::log::read_events,
    session::read_session_meta,
    tools::builtin_tools,
    turn::{run_turn, TurnResult},
};
use gait_identity::{DirectVerifier, IdentityContext, ProvenanceVerifier};
use identity_keyfile::testkit::TempAgentDir;

/// The substantive payload kind for a proto event, if it carries one.
fn event_kind(e: &Event) -> Option<&payload::Kind> {
    match &e.body {
        Some(event::Body::Payload(p)) => p.kind.as_ref(),
        _ => None,
    }
}
/// Test emitter: writes transcript, drops the live stream (receiver gone → sends ignored).
fn discard_emitter(gait_dir: &std::path::Path, id: &str) -> gait_core::Recorder {
    let (tx, _rx) =
        tokio::sync::mpsc::unbounded_channel::<gait_core::proto::gait::log::v1::StreamItem>();
    gait_core::Recorder::for_session(gait_dir, id, tx)
}

use async_stream::stream;
use gait_core::provider::{Provider, ProviderError, StreamChunk};
use gait_core::types::{Message, Tool};
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tempfile::TempDir;

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
        Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>,
        ProviderError,
    > {
        let i = self.idx.fetch_add(1, Ordering::SeqCst);
        let chunks = self.calls.get(i).cloned().unwrap_or_default();
        Ok(Box::pin(stream! { for c in chunks { yield Ok(c); } }))
    }
}

/// Verify that spawn_agent creates a child session with correct assigned_by and from fields.
#[tokio::test]
async fn spawn_agent_via_run_turn_creates_child_session() {
    let ws = TempDir::new().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let agent = TempAgentDir::new(now, 3_600);
    let identity = Arc::new(IdentityContext::new(agent.path()).unwrap());
    init_git_repo(ws.path());
    std::fs::create_dir_all(ws.path().join("inferences/mock")).unwrap();
    std::fs::write(
        ws.path().join("inferences/mock/inference.textproto"),
        "model: \"minimax/MiniMax-M3\"\ncompaction_threshold_tokens: 128000\n",
    )
    .unwrap();
    std::fs::write(
        ws.path().join("gait.jsonc"),
        r#"{"inference":"mock","allow":["spawn_agent"],"tools":[]}"#,
    )
    .unwrap();

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
        gait_core::tools::ToolCtx::new(
            ws.path().to_path_buf(),
            ws.path().to_path_buf(),
            "sparent001".into(),
        )
        .with_identity(Some(identity.clone())),
        vec![],
        vec![],
        vec![],
        gait_core::git::Repos::production(),
    );

    let result = run_turn(
        &gait_core::session::Session {
            id: "sparent001".into(),
            gait_dir: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            assigned_by: None,
            repos: gait_core::git::Repos::production(),
        },
        &discard_emitter(ws.path(), "sparent001"),
        gait_core::profile::ResolvedProfile::for_test(
            provider,
            tools,
            vec!["spawn_agent".into()],
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

    assert!(matches!(result, TurnResult::Done { .. }));

    // Find the child session by looking for session/* branches
    let repo = git2::Repository::open(ws.path()).unwrap();
    let mut child_id: Option<String> = None;
    for branch in repo
        .branches(Some(git2::BranchType::Local))
        .unwrap()
        .flatten()
    {
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

    // Verify child log's SessionStart has assigned_by = sparent001
    let meta = read_session_meta(ws.path(), &child_id).unwrap();
    assert_eq!(
        meta.assigned_by.as_deref(),
        Some("sparent001"),
        "child SessionStart must have assigned_by"
    );

    // Verify child transcript has user_turn signed by the parent's agent identity.
    let tx = gait_core::session::live::log_path(ws.path(), &child_id);
    let events = read_events(&tx).unwrap();
    let user_turn_event = events
        .iter()
        .find(|e| matches!(event_kind(e), Some(payload::Kind::UserText(_))))
        .expect("child transcript must have user_turn");
    let user_turn = match event_kind(user_turn_event) {
        Some(payload::Kind::UserText(u)) => u,
        _ => unreachable!(),
    };
    assert_eq!(user_turn.content, "fix the failing tests");
    let prov = user_turn_event
        .provenance
        .first()
        .expect("user_turn must carry provenance");
    assert!(
        prov.signed_message.is_some(),
        "child user_turn must carry a direct signature"
    );
    let verifier = DirectVerifier::from_context(&identity).unwrap();
    assert_eq!(
        verifier.verify_direct(user_turn_event, &child_id),
        Some(identity.agent_id().clone()),
        "spawn_agent must sign with a real, verifiable signature for the child session"
    );
}

/// Verify that run_turn returns TurnResult::Permissions when a tool call is blocked in non-auto mode.
#[tokio::test]
async fn run_turn_permissions_result_integration() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(ScriptedProvider {
        calls: Arc::new(vec![vec![
            StreamChunk::ToolCallComplete {
                tool_call_id: "tc1".into(),
                tool_name: "shell".into(),
                args: serde_json::json!({ "cmd": "rm -rf /", "reason": "cleanup" }),
            },
            StreamChunk::Done,
        ]]),
        idx: Arc::new(AtomicUsize::new(0)),
    });

    let result = run_turn(
        &gait_core::session::Session {
            id: "spermint".into(),
            gait_dir: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            assigned_by: None,
            repos: gait_core::git::Repos::production(),
        },
        &discard_emitter(ws.path(), "spermint"),
        gait_core::profile::ResolvedProfile::for_test(provider, vec![], vec![], vec![]),
        gait_core::turn::TurnInput {
            auto: false,
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

    assert!(
        matches!(result, TurnResult::Permissions),
        "expected Permissions result when blocked in non-auto mode"
    );
}
