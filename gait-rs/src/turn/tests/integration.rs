use super::super::*;
use crate::git::trace::{GitTraceRepo, TraceRepo};
use crate::provider::Provider;
use crate::provider::ProviderError;
use crate::session::log::read_events;
use crate::test_utils::init_git_repo;
use crate::tools::StructuredDoneTool;
use crate::types::ToolDef;
use async_stream::stream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

/// Bridges old per-field test call sites to the new run_turn(&Session, ResolvedProfile, TurnInput).
#[allow(clippy::too_many_arguments)]
async fn run_turn_test_call(
    gait_dir: std::path::PathBuf,
    codebase_path: std::path::PathBuf,
    session_id: String,
    provider: std::sync::Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    allow: Vec<String>,
    deny: Vec<String>,
    input: TurnInput,
    repos: crate::git::Repos,
) -> anyhow::Result<TurnResult> {
    let session = crate::session::Session {
        id: session_id,
        gait_dir,
        codebase_path,
        assigned_by: None,
        repos,
    };
    let caps = crate::profile::ResolvedProfile::for_test(provider, tools, allow, deny);
    let (tx, _rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let emitter = Recorder::for_session(&session.gait_dir, &session.id, tx);
    run_turn(&session, &emitter, caps, input).await
}

/// Like [`run_turn_test_call`], but first appends a `user_turn` (Phase 4
/// Option A: the caller owns input, `run_turn` folds). For tests that assert
/// the `user_turn` lands in the transcript.
#[allow(clippy::too_many_arguments)]
async fn run_turn_test_call_seeded(
    gait_dir: std::path::PathBuf,
    codebase_path: std::path::PathBuf,
    session_id: String,
    provider: std::sync::Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    allow: Vec<String>,
    deny: Vec<String>,
    task: &str,
    input: TurnInput,
    repos: crate::git::Repos,
) -> anyhow::Result<TurnResult> {
    let session = crate::session::Session {
        id: session_id,
        gait_dir,
        codebase_path,
        assigned_by: None,
        repos,
    };
    let caps = crate::profile::ResolvedProfile::for_test(provider, tools, allow, deny);
    let (tx, _rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let emitter = Recorder::for_session(&session.gait_dir, &session.id, tx);
    session
        .repos
        .traces
        .create_session_worktree(&session.gait_dir, &session.id)?;
    for body in crate::embed::user_turn_events(&session.repos, &session.codebase_path, task.into())
    {
        emitter.record(body)?;
    }
    run_turn(&session, &emitter, caps, input).await
}

struct MockProvider {
    responses: Arc<Vec<Vec<StreamChunk>>>,
    call_idx: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    fn model_id(&self) -> &str {
        "mock"
    }
    async fn stream(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[Box<dyn Tool>],
        _params: &crate::config::LlmParamsConfig,
        _tail_context: Option<&str>,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>,
        >,
        ProviderError,
    > {
        let idx = self.call_idx.fetch_add(1, Ordering::SeqCst);
        let chunks = self.responses.get(idx).map(|v| v.as_slice()).unwrap_or(&[]);
        let chunks_vec: Vec<StreamChunk> = chunks.to_vec();
        Ok(Box::pin(stream! {
            for c in chunks_vec { yield Ok(c); }
        }))
    }
}

/// One step in a scripted provider response. A `text` step produces a
/// text-only response (turn complete after this). The test must provide
/// enough steps to satisfy the model's stream loop.
#[derive(Debug, Clone)]
enum FakeStep {
    Text(String),
}

impl FakeStep {
    fn text(s: &str) -> Self {
        FakeStep::Text(s.into())
    }
}

/// Scripted provider that returns a deterministic sequence of steps.
/// Each `text` step is one model call (text → Done).
/// The test must provide enough steps to satisfy the model's stream loop.
struct FakeProvider {
    steps: Arc<Vec<FakeStep>>,
    call_idx: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
    fn model_id(&self) -> &str {
        "fake"
    }
    async fn stream(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[Box<dyn Tool>],
        _params: &crate::config::LlmParamsConfig,
        _tail_context: Option<&str>,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>,
        >,
        ProviderError,
    > {
        let idx = self.call_idx.fetch_add(1, Ordering::SeqCst);
        let step = self
            .steps
            .get(idx)
            .cloned()
            .unwrap_or(FakeStep::Text("".into()));
        let chunks: Vec<StreamChunk> = match step {
            FakeStep::Text(s) => vec![StreamChunk::Text(s), StreamChunk::Done],
        };
        Ok(Box::pin(stream! {
            for c in chunks { yield Ok(c); }
        }))
    }
}

#[tokio::test]
async fn run_turn_writes_transcript_and_commits() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    // Codebase dir (can be same as gait_dir for test)
    let codebase = ws.path().to_path_buf();

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            // Step 1: call the read tool
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(),
                    tool_name: "read".into(),
                    args: serde_json::json!({ "file_path": "README.md" }),
                },
                StreamChunk::Done,
            ],
            // Step 2: final text response
            vec![
                StreamChunk::Text("Task complete.".into()),
                StreamChunk::Done,
            ],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let read_tool = ToolDef {
        name: "read".into(),
        description: "read a file".into(),
        parameters: serde_json::json!({ "type": "object" }),
        execute: Box::new(|_args| {
            Box::pin(async move { Ok(serde_json::Value::String("file contents".into())) })
        }),
        permission: None,
    };

    run_turn_test_call_seeded(
        ws.path().to_path_buf(),
        codebase,
        "stest001".into(),
        provider,
        vec![Box::new(read_tool)],
        vec!["read".into()],
        vec![],
        "do the thing",
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    // Verify transcript has expected events
    let tx_path = ws.path().join("sessions/stest001/log.jsonl");
    let events = read_events(&tx_path).unwrap();
    let types: Vec<&str> = events
        .iter()
        .map(|e| match event_kind(e) {
            Some(payload::Kind::SessionStart(_)) => "session_start",
            Some(payload::Kind::UserText(_)) => "user_turn",
            Some(payload::Kind::ToolCall(_)) => "tool_call",
            Some(payload::Kind::ToolResult(_)) => "tool_result",
            Some(payload::Kind::AssistantText(_)) => "assistant_text",
            Some(payload::Kind::TurnComplete(_)) => "turn_complete",
            _ => "other",
        })
        .collect();
    assert!(types.contains(&"session_start"), "got: {:?}", types);
    assert!(types.contains(&"user_turn"), "got: {:?}", types);
    assert!(types.contains(&"tool_call"), "got: {:?}", types);
    assert!(types.contains(&"tool_result"), "got: {:?}", types);
    assert!(types.contains(&"turn_complete"), "got: {:?}", types);

    // Verify git commit was created
    let repo = git2::Repository::open(ws.path().join("sessions/stest001")).unwrap();
    let msg = repo
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .message()
        .unwrap()
        .to_string();
    assert!(msg.contains("turn 1"), "got: {msg}");
}

#[tokio::test]
async fn output_tool_result_used_in_turn_complete() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // output tool now exits immediately on ExecutionSignal::TurnComplete —
    // no second LLM call, no empty-text follow-up response.
    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::ToolCallComplete {
                tool_call_id: "tc1".into(),
                tool_name: "structured_done".into(),
                args: serde_json::json!({ "message": "fixed the bug", "result": { "files": 2 }}),
            },
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let output_tool = StructuredDoneTool::new();

    run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "stest_out".into(),
        provider,
        vec![output_tool.boxed()],
        vec![],
        vec![],
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = ws.path().join("sessions/stest_out/log.jsonl");
    let events = read_events(&tx_path).unwrap();
    let tc = events
        .iter()
        .find_map(|e| match event_kind(e) {
            Some(payload::Kind::TurnComplete(tc)) => Some(tc),
            _ => None,
        })
        .unwrap();
    let output = tc.output.as_ref().unwrap();
    assert_eq!(output.message, "fixed the bug");
    assert!(output.result.is_some());
}

#[tokio::test]
async fn output_tool_exempt_from_permission_gating() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // output tool now exits immediately on ExecutionSignal::TurnComplete.
    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::ToolCallComplete {
                tool_call_id: "tc1".into(),
                tool_name: "structured_done".into(),
                args: serde_json::json!({ "message": "done" }),
            },
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let output_tool = StructuredDoneTool::new();

    run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "stest_out2".into(),
        provider,
        vec![output_tool.boxed()],
        vec![],
        vec![],
        TurnInput {
            auto: false,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = ws.path().join("sessions/stest_out2/log.jsonl");
    let events = read_events(&tx_path).unwrap();
    let has_tc = events
        .iter()
        .any(|e| matches!(event_kind(e), Some(payload::Kind::TurnComplete(_))));
    let has_permissions = events
        .iter()
        .any(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))));
    assert!(has_tc, "output tool should complete the turn");
    assert!(
        !has_permissions,
        "output tool should not trigger permission gating"
    );
}

#[tokio::test]
async fn permission_gating_pauses_turn_in_non_auto_mode() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::ToolCallComplete {
                tool_call_id: "tc1".into(),
                tool_name: "shell".into(), // not in allow list
                args: serde_json::json!({ "cmd": "rm -rf /", "reason": "cleanup" }),
            },
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "stest002".into(),
        provider,
        vec![],
        vec!["read".into()],
        vec![],
        TurnInput {
            auto: false,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = ws.path().join("sessions/stest002/log.jsonl");
    let events = read_events(&tx_path).unwrap();
    let has_permissions = events
        .iter()
        .any(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))));
    let has_turn_complete = events
        .iter()
        .any(|e| matches!(event_kind(e), Some(payload::Kind::TurnComplete(_))));
    assert!(has_permissions, "should have permissions event");
    assert!(
        !has_turn_complete,
        "should NOT have turn_complete — turn was paused"
    );
}

#[tokio::test]
async fn run_turn_returns_done_result_on_completion() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::Text("I finished the task.".into()),
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let result = run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "sdone001".into(),
        provider,
        vec![],
        vec![],
        vec![],
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    assert!(
        matches!(result, TurnResult::Done { .. }),
        "expected Done, got: {:?}",
        result
    );
    if let TurnResult::Done { message, .. } = result {
        assert!(!message.is_empty(), "message should not be empty");
    }
}

#[tokio::test]
async fn run_turn_returns_permissions_when_paused() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::ToolCallComplete {
                tool_call_id: "tc1".into(),
                tool_name: "shell".into(),
                args: serde_json::json!({ "cmd": "ls", "reason": "listing" }),
            },
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let result = run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "sperm001".into(),
        provider,
        vec![],
        vec![],
        vec![],
        TurnInput {
            auto: false,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    assert!(
        matches!(result, TurnResult::Permissions),
        "expected Permissions, got: {:?}",
        result
    );
}

#[tokio::test]
async fn streaming_text_does_not_produce_extra_transcript_events() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::Text("hello ".into()),
            StreamChunk::Text("world".into()),
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "sdelta01".into(),
        provider,
        vec![],
        vec![],
        vec![],
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = ws.path().join("sessions/sdelta01/log.jsonl");
    let events = read_events(&tx_path).unwrap();
    let text_events: Vec<_> = events
        .iter()
        .filter_map(|e| match event_kind(e) {
            Some(payload::Kind::AssistantText(a)) => Some(a),
            _ => None,
        })
        .collect();
    assert_eq!(text_events.len(), 1);
    assert_eq!(text_events[0].content, "hello world");
}

#[tokio::test]
async fn denied_tool_gets_blocked_result_without_prompting() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // MockProvider: step 1 = shell rm call, step 2 = output with summary
    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(),
                    tool_name: "shell".into(),
                    args: serde_json::json!({ "cmd": "rm -rf /", "reason": "cleanup" }),
                },
                StreamChunk::Done,
            ],
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc2".into(),
                    tool_name: "structured_done".into(),
                    args: serde_json::json!({ "message": "blocked" }),
                },
                StreamChunk::Done,
            ],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let shell_tool = crate::tools::ShellTool::new(crate::tools::ToolCtx::new(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "test".into(),
    ));
    let output_tool = StructuredDoneTool::new();

    run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "sdeny001".into(),
        provider,
        vec![shell_tool.boxed(), output_tool.boxed()],
        vec![],
        vec!["shell:rm *".to_string()],
        TurnInput {
            auto: false,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = ws.path().join("sessions/sdeny001/log.jsonl");
    let events = read_events(&tx_path).unwrap();

    // Must have a ToolResult indicating blocked by config
    let blocked_result = events.iter().any(|e| {
        matches!(event_kind(e), Some(payload::Kind::ToolResult(tr))
        if tr.call_id == "tc1" && tr.output.contains("Blocked by config"))
    });
    assert!(
        blocked_result,
        "expected blocked ToolResult for tc1; events: {:?}",
        events
    );

    // NO Permissions event should be emitted (deny is a hard floor, not a prompt)
    let has_permissions = events
        .iter()
        .any(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))));
    assert!(
        !has_permissions,
        "deny should not trigger a Permissions event"
    );
}

#[tokio::test]
async fn deny_wins_when_tool_in_both_allow_and_deny() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(),
                    tool_name: "shell".into(),
                    args: serde_json::json!({ "cmd": "rm -rf /", "reason": "cleanup" }),
                },
                StreamChunk::Done,
            ],
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc2".into(),
                    tool_name: "structured_done".into(),
                    args: serde_json::json!({ "message": "blocked" }),
                },
                StreamChunk::Done,
            ],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let shell_tool = crate::tools::ShellTool::new(crate::tools::ToolCtx::new(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "test".into(),
    ));
    let output_tool = StructuredDoneTool::new();

    run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "sdeny002".into(),
        provider,
        vec![shell_tool.boxed(), output_tool.boxed()],
        vec!["shell:rm *".to_string()],
        vec!["shell:rm *".to_string()],
        TurnInput {
            auto: false,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = ws.path().join("sessions/sdeny002/log.jsonl");
    let events = read_events(&tx_path).unwrap();

    // Same assertions as the first deny test
    let blocked_result = events.iter().any(|e| {
        matches!(event_kind(e), Some(payload::Kind::ToolResult(tr))
        if tr.call_id == "tc1" && tr.output.contains("Blocked by config"))
    });
    assert!(
        blocked_result,
        "expected blocked ToolResult for tc1 even with allow match; events: {:?}",
        events
    );

    let has_permissions = events
        .iter()
        .any(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))));
    assert!(
        !has_permissions,
        "deny-wins: should not trigger Permissions event"
    );
}

#[tokio::test]
async fn resume_executes_approved_tool_directly() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let codebase = TempDir::new().unwrap();
    let session_id = "resume-exec-test";
    // Initialize the session worktree (creates the session dir + branch)
    GitTraceRepo
        .create_session_worktree(ws.path(), session_id)
        .unwrap();
    let session_dir = ws.path().join("sessions").join(session_id);
    let tx_path = session_dir.join("log.jsonl");

    // Pre-populate transcript: SessionStart + UserTurn + Permissions + PermissionsReply
    let perm_item = pb::PermissionItem {
        id: "tc-resume-1".into(),
        cmd: "echo hello".into(),
        reason: "shell requires permission".into(),
        patterns: vec!["echo *".into()],
        tool_name: "shell".into(),
        tool_args: crate::proto::events::to_struct(
            &serde_json::json!({"cmd": "echo hello", "reason": "test"}),
        ),
        preview: None,
    };
    wire::append(&tx_path, events::session_start(session_id.into(), None)).unwrap();
    wire::append(&tx_path, events::user_text("run echo".into(), None)).unwrap();
    wire::append(&tx_path, events::permissions(&[perm_item])).unwrap();
    wire::append(
        &tx_path,
        events::permissions_reply(
            "user".into(),
            &[pb::PermissionsReplyItem {
                id: "tc-resume-1".into(),
                decision: pb::Decision::Allow as i32,
                text: Some("shell:echo *".into()),
                scope: pb::Scope::Unspecified as i32,
            }],
        ),
    )
    .unwrap();

    let shell_tool = ToolDef {
        name: "shell".into(),
        description: "run a shell command".into(),
        parameters: serde_json::json!({ "type": "object" }),
        execute: Box::new(|_args| {
            Box::pin(async move { Ok(serde_json::Value::String("hello\n".into())) })
        }),
        permission: None,
    };
    let provider = Arc::new(FakeProvider {
        steps: Arc::new(vec![FakeStep::text("done")]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });
    let result = run_turn_test_call(
        ws.path().to_path_buf(),
        codebase.path().to_path_buf(),
        session_id.into(),
        provider,
        vec![Box::new(shell_tool)],
        vec!["shell:echo *".into()],
        vec![],
        TurnInput {
            auto: false,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    assert!(
        matches!(result, TurnResult::Done { .. }),
        "expected Done, got: {:?}",
        result
    );

    let events = read_events(&tx_path).unwrap();
    // ToolCall + ToolResult must appear (from direct execution, not re-request)
    assert!(
        events.iter().any(
            |e| matches!(event_kind(e), Some(payload::Kind::ToolCall(tc)) if tc.name == "shell")
        ),
        "expected ToolCall from resume"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(event_kind(e), Some(payload::Kind::ToolResult(_)))),
        "expected ToolResult from resume"
    );
    // No extra Permissions event (tool was executed, not gated again)
    let perm_count = events
        .iter()
        .filter(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))))
        .count();
    assert_eq!(
        perm_count, 1,
        "should have only the pre-existing Permissions event"
    );
}

#[tokio::test]
async fn resume_emits_denied_tool_result_for_rejected_call() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let codebase = TempDir::new().unwrap();
    let session_id = "resume-deny-test";
    // Initialize the session worktree (creates the session dir + branch)
    GitTraceRepo
        .create_session_worktree(ws.path(), session_id)
        .unwrap();
    let session_dir = ws.path().join("sessions").join(session_id);
    let tx_path = session_dir.join("log.jsonl");

    let perm_item = pb::PermissionItem {
        id: "tc-deny-1".into(),
        cmd: "rm -rf /".into(),
        reason: "shell requires permission".into(),
        patterns: vec!["rm *".into()],
        tool_name: "shell".into(),
        tool_args: crate::proto::events::to_struct(
            &serde_json::json!({"cmd": "rm -rf /", "reason": "cleanup"}),
        ),
        preview: None,
    };
    wire::append(&tx_path, events::session_start(session_id.into(), None)).unwrap();
    wire::append(&tx_path, events::user_text("clean up".into(), None)).unwrap();
    wire::append(&tx_path, events::permissions(&[perm_item])).unwrap();
    wire::append(
        &tx_path,
        events::permissions_reply(
            "user".into(),
            &[pb::PermissionsReplyItem {
                id: "tc-deny-1".into(),
                decision: pb::Decision::Deny as i32,
                text: None,
                scope: pb::Scope::Unspecified as i32,
            }],
        ),
    )
    .unwrap();

    let shell_tool = ToolDef {
        name: "shell".into(),
        description: "run a shell command".into(),
        parameters: serde_json::json!({ "type": "object" }),
        execute: Box::new(|_args| {
            Box::pin(async move { Ok(serde_json::Value::String("should not run".into())) })
        }),
        permission: None,
    };
    let provider = Arc::new(FakeProvider {
        steps: Arc::new(vec![FakeStep::text("ok I won't do that")]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });
    let result = run_turn_test_call(
        ws.path().to_path_buf(),
        codebase.path().to_path_buf(),
        session_id.into(),
        provider,
        vec![Box::new(shell_tool)],
        vec![],
        vec![],
        TurnInput {
            auto: false,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    assert!(
        matches!(result, TurnResult::Done { .. }),
        "expected Done, got: {:?}",
        result
    );

    // Must have a ToolResult for the denied call (API invariant)
    let events_after = read_events(&tx_path).unwrap();
    let denied_result = events_after.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::ToolResult(tr)) if tr.call_id == "tc-deny-1"));
    assert!(denied_result, "expected ToolResult for denied call");
    if let Some(payload::Kind::ToolResult(tr)) =
        events_after.iter().find_map(|e| match event_kind(e) {
            Some(payload::Kind::ToolResult(tr)) if tr.call_id == "tc-deny-1" => {
                Some(payload::Kind::ToolResult(tr))
            }
            _ => None,
        })
    {
        assert!(tr.output.contains("denied"), "got: {}", tr.output);
    }

    // Reconstruction must yield paired messages — not drop the orphan ToolResult.
    // Without a preceding ToolCall the ToolResult is silently skipped by context.rs,
    // so the model never sees the denial and retries the tool call in a loop.
    let messages = retrace::messages(&events_after).unwrap();
    // Expect: User (UserTurn), Assistant (ToolCall), Tool (ToolResult with denial)
    let has_tool_msg = messages
        .iter()
        .any(|m| matches!(m, Message::ToolResult { .. }));
    assert!(
        has_tool_msg,
        "reconstruction must include Tool message with denied result; got {} messages",
        messages.len()
    );
}

#[tokio::test]
async fn resume_with_scope_agent_shows_whitelist_prefix() {
    // When a user approved with scope="agent", the resumed tool result should
    // show "[User added to whitelist: <pattern>]" instead of "[User approved.]"
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let session_id = "sscope001";
    // Initialize the session worktree (creates the session dir + branch)
    GitTraceRepo
        .create_session_worktree(ws.path(), session_id)
        .unwrap();
    let tx_path = ws
        .path()
        .join("sessions")
        .join(session_id)
        .join("log.jsonl");

    // Pre-write a transcript with a Permissions+PermissionsReply already in it,
    // simulating a previous turn that was interrupted mid-permission-prompt.
    // The PermissionsReply has scope="agent" to indicate it was saved to agent config.
    let shell_args = serde_json::json!({ "cmd": "ls", "reason": "list files" });
    wire::append(&tx_path, events::session_start(session_id.into(), None)).unwrap();
    wire::append(&tx_path, events::user_text("list files".into(), None)).unwrap();
    wire::append(
        &tx_path,
        events::permissions(&[pb::PermissionItem {
            id: "tc-scope-1".into(),
            cmd: "ls".into(),
            reason: "shell requires permission".into(),
            patterns: vec!["shell:ls *".to_string()],
            tool_name: "shell".into(),
            tool_args: crate::proto::events::to_struct(&shell_args),
            preview: None,
        }]),
    )
    .unwrap();
    wire::append(
        &tx_path,
        events::permissions_reply(
            "user".into(),
            &[pb::PermissionsReplyItem {
                id: "tc-scope-1".into(),
                decision: pb::Decision::Allow as i32,
                text: None,
                scope: pb::Scope::Agent as i32,
            }],
        ),
    )
    .unwrap();

    let shell_tool = ToolDef {
        name: "shell".into(),
        description: "run a shell command".into(),
        parameters: serde_json::json!({ "type": "object" }),
        execute: Box::new(|_args| {
            Box::pin(async move { Ok(serde_json::Value::String("file1\nfile2\n".into())) })
        }),
        permission: None,
    };

    // After resuming and executing shell, agent calls output to complete
    let provider = Arc::new(FakeProvider {
        steps: Arc::new(vec![FakeStep::text("done")]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        session_id.into(),
        provider,
        vec![Box::new(shell_tool)],
        vec![],
        vec![],
        TurnInput {
            auto: false,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let events = read_events(&tx_path).unwrap();
    // The shell tool result emitted during resume should contain the whitelist prefix
    let whitelist_result = events.iter().any(|e| {
        if let Some(payload::Kind::ToolResult(tr)) = event_kind(e) {
            tr.call_id == "tc-scope-1" && tr.output.contains("[User added to whitelist:")
        } else {
            false
        }
    });
    assert!(
        whitelist_result,
        "agent-scope approval should show whitelist prefix; events: {:?}",
        events
    );
}

#[tokio::test]
async fn run_turn_records_code_checkpoint_for_git_codebase() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // Build a SEPARATE codebase dir that is its own git repo with one commit
    let codebase_dir = TempDir::new().unwrap();
    {
        let repo = git2::Repository::init(codebase_dir.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        std::fs::write(codebase_dir.path().join("hello.txt"), "hello\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("hello.txt")).unwrap();
        idx.write().unwrap();
        let tree_id = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    let session_id = "scpgit001";

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::Text("done".into()),
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    run_turn_test_call(
        ws.path().to_path_buf(),
        codebase_dir.path().to_path_buf(),
        session_id.into(),
        provider,
        vec![],
        vec![],
        vec![],
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = ws
        .path()
        .join("sessions")
        .join(session_id)
        .join("log.jsonl");
    let events = read_events(&tx_path).unwrap();
    let cp = events
        .iter()
        .rev()
        .find_map(|e| match event_kind(e) {
            Some(payload::Kind::TurnComplete(tc)) => Some(tc.checkpoint.clone()),
            _ => None,
        })
        .flatten();
    assert!(cp.is_some(), "expected a code checkpoint SHA");
    assert!(crate::git::checkpoint_git_dir(ws.path(), session_id).exists());
}

#[tokio::test]
async fn run_turn_persists_codebase_path_in_session_json() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // Build a SEPARATE codebase dir that is its own git repo with one commit
    let codebase_dir = TempDir::new().unwrap();
    {
        let repo = git2::Repository::init(codebase_dir.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        std::fs::write(codebase_dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("main.rs")).unwrap();
        idx.write().unwrap();
        let tree_id = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    let session_id = "scbpath001";

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::Text("all done".into()),
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    run_turn_test_call(
        ws.path().to_path_buf(),
        codebase_dir.path().to_path_buf(),
        session_id.into(),
        provider,
        vec![],
        vec![],
        vec![],
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    // After the turn, session.json should have codebase_path persisted
    use crate::session::session_json::read_session_json;
    let sj = read_session_json(ws.path(), session_id).unwrap();
    assert!(
        sj.codebase_path.is_some(),
        "codebase_path should be persisted in session.json"
    );
    assert_eq!(
        sj.codebase_path.as_deref(),
        Some(codebase_dir.path().to_string_lossy().as_ref()),
        "codebase_path value should match the input"
    );
}

#[tokio::test]
async fn run_turn_calls_rewind_init_and_checkpoint_via_injected_repo() {
    use crate::git::checkpoint::MockCheckpointRepo;

    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // Build a SEPARATE codebase dir that is its own git repo with one commit
    let codebase_dir = TempDir::new().unwrap();
    {
        let repo = git2::Repository::init(codebase_dir.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        std::fs::write(codebase_dir.path().join("hello.txt"), "hello\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("hello.txt")).unwrap();
        idx.write().unwrap();
        let tree_id = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    let session_id = "rewindinject001";

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::Text("done".into()),
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let mock = Arc::new(MockCheckpointRepo::new());
    let repos = crate::git::Repos {
        codebase: Arc::new(crate::git::codebase::GitCodebaseRepo),
        traces: Arc::new(crate::git::trace::GitTraceRepo),
        checkpoints: mock.clone(),
    };

    run_turn_test_call(
        ws.path().to_path_buf(),
        codebase_dir.path().to_path_buf(),
        session_id.into(),
        provider,
        vec![],
        vec![],
        vec![],
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        repos,
    )
    .await
    .unwrap();

    assert_eq!(
        mock.init_calls().len(),
        1,
        "rewind.init should be called exactly once at session start"
    );
    assert_eq!(
        mock.checkpoint_calls(),
        vec![(session_id.to_string(), 1u32)],
        "rewind.checkpoint should be called for turn 1"
    );
}

#[tokio::test]
async fn load_skill_emits_toggle_event() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(),
                    tool_name: "enable".into(),
                    args: serde_json::json!({ "name": "tdd", "enabled": true }),
                },
                StreamChunk::Done,
            ],
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc2".into(),
                    tool_name: "structured_done".into(),
                    args: serde_json::json!({ "message": "done" }),
                },
                StreamChunk::Done,
            ],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let skill = crate::config::SkillDef {
        name: "tdd".into(),
        description: String::new(),
        content: "TDD skill content".into(),
        enabled: false,
    };
    let load_tool = crate::tools::LoadTool::new(vec![], vec![skill], vec![]);
    let output_tool = StructuredDoneTool::new();

    run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "slogtgl01".into(),
        provider,
        vec![Box::new(load_tool), Box::new(output_tool)],
        vec!["enable".into()],
        vec![],
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = ws.path().join("sessions/slogtgl01/log.jsonl");
    let events = crate::proto::wire::read_events(&tx_path).unwrap();
    let toggles: Vec<_> = events
        .iter()
        .filter_map(|e| match event_kind(e) {
            Some(payload::Kind::Toggle(t)) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(toggles.len(), 1, "exactly one Toggle event emitted");
    assert_eq!(toggles[0].name, "tdd");
    assert!(toggles[0].enabled, "Toggle enabled=true");
    use crate::proto::gait::log::v1::ToggleKind;
    assert_eq!(
        toggles[0].kind,
        ToggleKind::Skill as i32,
        "skill kind — not in mcp_registry"
    );
}

#[tokio::test]
async fn always_policy_streams_edit_preview_delta() {
    use crate::config::PreviewPolicy;
    use crate::proto::gait::log::v1::stream_item::Item;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(&path, "fn foo() {\n    bar();\n}").unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.split('\n').collect();
    let hash = crate::hashline::compute_line_hash(2, lines[1]);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let emitter = crate::proto::Recorder::new(dir.path().join("log.jsonl"), tx);
    let ctx = crate::tools::ToolCtx::new(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        "t".into(),
    );
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::EditTool::new(ctx))];
    let args = serde_json::json!({
        "file_path": "f.rs",
        "edits": [{"op": "replace", "pos": hash, "lines": ["    baz();"]}] // bare hash, no "2#"
    });

    stream_preview_if_always(
        PreviewPolicy::Always,
        tools
            .iter()
            .find(|t| t.name() == "edit")
            .map(|t| t.as_ref()),
        &args,
        &emitter,
    );

    // No transcript write (ephemeral).
    assert!(!dir.path().join("log.jsonl").exists());
    // A Delta carrying the diff was streamed.
    let item = rx.try_recv().expect("a delta").item.unwrap();
    let Item::Delta(d) = item else {
        panic!("expected Delta")
    };
    assert!(
        d.content.contains("-    bar();") && d.content.contains("+    baz();"),
        "delta: {}",
        d.content
    );
}

#[tokio::test]
async fn on_ask_policy_streams_nothing() {
    use crate::config::PreviewPolicy;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.rs"), "fn foo() {}").unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let emitter = crate::proto::Recorder::new(dir.path().join("log.jsonl"), tx);
    let ctx = crate::tools::ToolCtx::new(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        "t".into(),
    );
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::EditTool::new(ctx))];
    let args = serde_json::json!({"file_path": "f.rs", "edits": []});

    stream_preview_if_always(
        PreviewPolicy::OnAsk,
        tools
            .iter()
            .find(|t| t.name() == "edit")
            .map(|t| t.as_ref()),
        &args,
        &emitter,
    );
    assert!(rx.try_recv().is_err(), "on-ask must stream nothing");
}

// ── Task 5: supervised mode scope pre-check ────────────────────────────────────

/// Supervised mode: write tool targeting a path outside the sandbox ceiling
/// (e.g. /etc/x, which is outside the codebase root) must return
/// TurnResult::Unlock with one item naming that path, and the file must NOT
/// be written.
#[tokio::test]
async fn supervised_write_outside_ceiling_pauses_with_unlock() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // The model issues a write to /etc/x — outside the codebase ceiling.
    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::ToolCallComplete {
                tool_call_id: "tc-scope-write".into(),
                tool_name: "write".into(),
                args: serde_json::json!({
                    "file_path": "/etc/x",
                    "content": "should not be written"
                }),
            },
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let write_tool = crate::tools::WriteTool::new(crate::tools::ToolCtx::new(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "test".into(),
    ));

    let result = run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "sscope-write-01".into(),
        provider,
        vec![write_tool.boxed()],
        // write is in the allow list so permission gating won't trigger first
        vec!["write".into()],
        vec![],
        TurnInput {
            auto: false, // supervised mode — pre-check must pause
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    // Must return TurnResult::Unlock, not Done/Permissions.
    match &result {
        TurnResult::Unlock { items } => {
            assert_eq!(items.len(), 1, "expected one unlock item, got {items:?}");
            let item = &items[0];
            assert!(
                item.resource.contains("/etc/x"),
                "unlock item resource should name /etc/x, got: {}",
                item.resource
            );
            assert_eq!(item.tool_name, "write", "tool_name should be write");
            assert_eq!(item.dimension, "write", "dimension should be write");
        }
        other => panic!("expected TurnResult::Unlock, got: {other:?}"),
    }

    // The file must NOT have been written.
    assert!(
        !std::path::Path::new("/etc/x").exists(),
        "/etc/x must not be created by the pre-check"
    );
}

/// Regression test for I1 (canonicalization mismatch): a supervised write to a
/// relative in-codebase path (e.g. `"foo.txt"`) must NOT trigger an Unlock pause.
///
/// On macOS `tempfile::TempDir` lives under `/tmp`, which is a symlink to
/// `/private/tmp`. `compute_scope` canonicalizes the codebase path to
/// `/private/tmp/...`, but without the fix in `check_own_tool_scope` the
/// resolved path `codebase.join("foo.txt")` is `/tmp/.../foo.txt` and fails
/// `starts_with(/private/tmp/...)`, spuriously returning `TurnResult::Unlock`.
#[tokio::test]
async fn supervised_write_inside_codebase_returns_done() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // The model writes "foo.txt" — a relative path inside the codebase.
    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc-scope-write-rel".into(),
                    tool_name: "write".into(),
                    args: serde_json::json!({
                        "file_path": "foo.txt",
                        "content": "hello from in-codebase write"
                    }),
                },
                StreamChunk::Done,
            ],
            // After write completes, model finishes with a text response.
            vec![StreamChunk::Text("wrote foo.txt".into()), StreamChunk::Done],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let write_tool = crate::tools::WriteTool::new(crate::tools::ToolCtx::new(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "test".into(),
    ));

    let result = run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "sscope-write-inbounds-01".into(),
        provider,
        vec![write_tool.boxed()],
        // write is in the allow list so permission gating won't trigger first
        vec!["write".into()],
        vec![],
        TurnInput {
            auto: false, // supervised mode — in-codebase write must NOT pause
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    // Must NOT return TurnResult::Unlock — the write is inside the codebase.
    assert!(
        matches!(result, TurnResult::Done { .. }),
        "in-codebase write must return Done, not Unlock (canonicalization bug); got: {result:?}"
    );

    // The file must actually have been written.
    let written_path = ws.path().join("foo.txt");
    assert!(
        written_path.exists(),
        "foo.txt must be written to the codebase dir"
    );
    let contents = std::fs::read_to_string(&written_path).unwrap();
    assert_eq!(
        contents, "hello from in-codebase write",
        "foo.txt contents must match what the model wrote"
    );
}

// ── Task 6: shell EACCES → unlock (supervised) / failure (auto) ──────────────

/// Auto mode: a shell command whose output contains "Permission denied" must
/// NOT trigger an unlock pause — the tool failure propagates normally and the
/// turn completes. This test runs in-process (no gait binary required) and
/// verifies the auto-mode guard in the turn loop.
///
/// Note: in the test environment the binary-name guard detects a non-`gait`
/// test harness and falls back to `InProcessSynapse`, which always returns
/// `denied = None`. The auto-mode assertion here is therefore testing that
/// the loop handles `denied = None` in auto mode correctly (no-op) — the
/// supervised-mode path is covered by the `#[ignore]` test below.
#[tokio::test]
async fn auto_mode_shell_denial_propagates_as_tool_failure() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // The model: (1) issues a shell command, (2) finishes after seeing the result.
    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc-shell-deny".into(),
                    tool_name: "shell".into(),
                    args: serde_json::json!({
                        "command": "echo 'Permission denied' && exit 1"
                    }),
                },
                StreamChunk::Done,
            ],
            vec![StreamChunk::Text("done".into()), StreamChunk::Done],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let shell_tool = crate::tools::ShellTool::new(crate::tools::ToolCtx::new(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "test".into(),
    ));

    let result = run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "sshell-auto-deny-01".into(),
        provider,
        vec![shell_tool.boxed()],
        vec!["shell".into()],
        vec![],
        TurnInput {
            auto: true, // auto mode — denial must NOT pause
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    // In auto mode with InProcessSynapse (test environment) the turn always
    // continues — even if the output contains "Permission denied". The denial
    // scanning only triggers via the real hand subprocess (`gait __run-tool`).
    assert!(
        matches!(result, TurnResult::Done { .. }),
        "auto mode must continue after tool failure, not pause; got: {result:?}"
    );
}

/// Supervised mode: a sealed `shell` command that attempts an out-of-scope write
/// (e.g. `echo hi > /etc/gait-probe`) causes the hand to detect EACCES/EPERM
/// in the output and set `Feedback.denied`. The turn loop must then return
/// `TurnResult::Unlock` instead of continuing.
///
/// **Requires the `gait` binary to be built first** (the hand is `gait __run-tool`):
///
///     cargo build -p gait-bin
///     cargo test -p gait-core -- --ignored shell_eacces_supervised_pause
///
/// On macOS the Seatbelt seal enforces the out-of-scope write denial (EPERM).
/// On Linux the test is also useful for Landlock, but `shell` writing to `/etc`
/// may fail due to OS-level permissions rather than Seatbelt, which still
/// produces "Permission denied" in stderr and satisfies the detection logic.
#[tokio::test]
#[ignore = "requires `gait` binary + Seatbelt seal on macOS; build with `cargo build -p gait-bin`"]
#[cfg(target_os = "macos")]
async fn shell_eacces_supervised_pause() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // The model issues a shell write to /etc — EPERM under Seatbelt seal.
    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::ToolCallComplete {
                tool_call_id: "tc-eacces".into(),
                tool_name: "shell".into(),
                args: serde_json::json!({
                    "command": "echo hi > /etc/gait-probe"
                }),
            },
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let shell_tool = crate::tools::ShellTool::new(crate::tools::ToolCtx::new(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "test".into(),
    ));

    let result = run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "sshell-eacces-sup-01".into(),
        provider,
        vec![shell_tool.boxed()],
        vec!["shell".into()],
        vec![],
        TurnInput {
            auto: false, // supervised — denial must pause
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    match &result {
        TurnResult::Unlock { items } => {
            assert!(!items.is_empty(), "expected at least one unlock item");
            let item = &items[0];
            assert_eq!(item.tool_name, "shell", "tool_name should be shell");
            assert_eq!(item.dimension, "write", "dimension should be write");
        }
        other => panic!("expected TurnResult::Unlock (shell EACCES → denied), got: {other:?}"),
    }
}

// ── Task 4 (Phase 2d): supervised read of out-of-scope path → Unlock ─────────

/// Supervised mode: when the model issues a `read` of an absolute path that lies
/// outside the sandbox ceiling (i.e. not under the codebase root), the pre-check
/// must return `TurnResult::Unlock` with `dimension = "read"`.
///
/// This validates that Phase 2d's `read_all = false` default wires through
/// `check_own_tool_scope` → `ReadTool::scope_targets` → `Dimension::Read` check
/// correctly: an out-of-scope read is caught by the pre-check rather than
/// executing and returning file contents.
///
/// We use a scratch file under `/tmp/gait-test-oos-read-<pid>` that lives outside
/// the TempDir codebase root (which is also under `/tmp` but at a different path),
/// so the file is real but the path is provably out of scope for the session.
#[tokio::test]
async fn supervised_read_outside_ceiling_pauses_with_unlock() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // Create a real file outside the codebase tree.
    let oos_path =
        std::path::PathBuf::from(format!("/tmp/gait-test-oos-read-{}", std::process::id()));
    std::fs::write(&oos_path, "secret content\n").expect("failed to write oos sentinel file");

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![vec![
            StreamChunk::ToolCallComplete {
                tool_call_id: "tc-read-oos".into(),
                tool_name: "read".into(),
                args: serde_json::json!({ "file_path": oos_path.to_str().unwrap() }),
            },
            StreamChunk::Done,
        ]]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let read_tool = crate::tools::ReadTool::new(crate::tools::ToolCtx::new(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "test".into(),
    ));

    let result = run_turn_test_call(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "sread-oos-01".into(),
        provider,
        vec![read_tool.boxed()],
        // read is in the allow list so permission gating won't intercept first
        vec!["read".into()],
        vec![],
        TurnInput {
            auto: false, // supervised mode — pre-check must pause
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    // Cleanup sentinel file regardless of outcome.
    let _ = std::fs::remove_file(&oos_path);

    // Must return TurnResult::Unlock with dimension="read".
    match &result {
        TurnResult::Unlock { items } => {
            assert_eq!(items.len(), 1, "expected one unlock item, got {items:?}");
            let item = &items[0];
            assert_eq!(item.tool_name, "read", "tool_name should be 'read'");
            assert_eq!(item.dimension, "read", "dimension should be 'read'");
            assert!(
                item.resource.contains("gait-test-oos-read"),
                "resource should name the out-of-scope path, got: {}",
                item.resource
            );
        }
        other => panic!("expected TurnResult::Unlock (read out-of-scope), got: {other:?}"),
    }
}
