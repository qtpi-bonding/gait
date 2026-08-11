use super::super::*;
use crate::git::trace::{GitTraceRepo, TraceRepo};
use crate::proto::wire;
use crate::session::log::read_events;
use crate::session::retrace;
use crate::test_utils::init_git_repo;
use crate::tools::StructuredDoneTool;
use crate::types::ToolDef;
use async_stream::stream;
use gait_provider::provider::Provider;
use gait_provider::provider::ProviderError;
use gait_provider::provider::StreamChunk;
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
    // create_session_worktree's orphan tree starts empty -- the nested
    // sessions/<id>/ dir (see crate::session::live) doesn't exist until the
    // session's first write. run_turn's own setup (init_session) would
    // normally create it, but this helper writes user_turn events directly
    // via emitter.record() BEFORE run_turn runs, so it must create the
    // directory itself first.
    std::fs::create_dir_all(
        crate::session::live::log_path(&session.gait_dir, &session.id)
            .parent()
            .unwrap(),
    )?;
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
async fn run_turn_writes_brain_events_without_coordinator_lifecycle() {
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    // Verify transcript has expected events
    let tx_path = crate::session::live::log_path(ws.path(), "stest001");
    let events = read_events(&tx_path).unwrap();
    let types: Vec<&str> = events
        .iter()
        .map(|e| match event_kind(e) {
            Some(payload::Kind::SessionStart(_)) => "session_start",
            Some(payload::Kind::UserText(_)) => "user_turn",
            Some(payload::Kind::ToolCall(_)) => "tool_call",
            Some(payload::Kind::ToolResult(_)) => "tool_result",
            Some(payload::Kind::AssistantText(_)) => "assistant_text",
            Some(payload::Kind::RunComplete(_)) => "run_complete",
            _ => "other",
        })
        .collect();
    assert!(types.contains(&"session_start"), "got: {:?}", types);
    assert!(types.contains(&"user_turn"), "got: {:?}", types);
    assert!(types.contains(&"tool_call"), "got: {:?}", types);
    assert!(types.contains(&"tool_result"), "got: {:?}", types);
    assert!(
        !types.contains(&"run_complete"),
        "the brain primitive must leave durable lifecycle finalization to the coordinator: {types:?}"
    );
}

#[tokio::test]
async fn run_turn_brackets_each_step_with_llm_call_events() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let codebase = ws.path().to_path_buf();

    let provider = Arc::new(FakeProvider {
        steps: Arc::new(vec![FakeStep::text("done")]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    run_turn_test_call_seeded(
        ws.path().to_path_buf(),
        codebase,
        "stest-llm-events".into(),
        provider,
        vec![],
        vec![],
        vec![],
        "say hi",
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = crate::session::live::log_path(ws.path(), "stest-llm-events");
    let events = read_events(&tx_path).unwrap();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match event_kind(e) {
            Some(payload::Kind::LlmCallStarted(_)) => "llm_call_started",
            Some(payload::Kind::LlmCallCompleted(_)) => "llm_call_completed",
            _ => "other",
        })
        .collect();
    let started_idx = kinds.iter().position(|k| *k == "llm_call_started");
    let completed_idx = kinds.iter().position(|k| *k == "llm_call_completed");
    assert!(
        started_idx.is_some() && completed_idx.is_some(),
        "expected one LlmCallStarted and one LlmCallCompleted, got: {:?}",
        kinds
    );
    assert!(
        started_idx.unwrap() < completed_idx.unwrap(),
        "LlmCallStarted must precede LlmCallCompleted, got: {:?}",
        kinds
    );

    let completed = events
        .iter()
        .find_map(|e| match event_kind(e) {
            Some(payload::Kind::LlmCallCompleted(c)) => Some(c),
            _ => None,
        })
        .unwrap();
    assert!(completed.had_text, "the fake provider yielded text");
    assert_eq!(completed.tool_call_count, 0);
}

#[tokio::test]
async fn output_tool_result_used_in_turn_result() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // output tool now exits immediately on ExecutionSignal::FinishTurn —
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

    let result = run_turn_test_call(
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    match result {
        crate::turn::TurnResult::Done {
            message, result, ..
        } => {
            assert_eq!(message, "fixed the bug");
            assert!(result.is_some());
        }
        other => panic!("expected structured done, got {other:?}"),
    }
}

#[tokio::test]
async fn output_tool_exempt_from_permission_gating() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // output tool now exits immediately on ExecutionSignal::FinishTurn.
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

    let result = run_turn_test_call(
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    assert!(
        matches!(result, crate::turn::TurnResult::Done { .. }),
        "output tool should complete the brain turn"
    );

    let tx_path = crate::session::live::log_path(ws.path(), "stest_out2");
    let events = read_events(&tx_path).unwrap();
    let has_permissions = events
        .iter()
        .any(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))));
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = crate::session::live::log_path(ws.path(), "stest002");
    let events = read_events(&tx_path).unwrap();
    let has_permissions = events
        .iter()
        .any(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))));
    let has_turn_result = events
        .iter()
        .any(|e| matches!(event_kind(e), Some(payload::Kind::RunComplete(_))));
    assert!(has_permissions, "should have permissions event");
    assert!(
        !has_turn_result,
        "should NOT have turn_result — turn was paused"
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
            initial_git: None,
            daemons: vec![],
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
            initial_git: None,
            daemons: vec![],
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = crate::session::live::log_path(ws.path(), "sdelta01");
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = crate::session::live::log_path(ws.path(), "sdeny001");
    let events = read_events(&tx_path).unwrap();

    // Must have a PermissionDenied event for tc1's tool, and its ToolResult
    // must NOT contain the retired "[Blocked by config:" string convention.
    let permission_denied = events.iter().any(|e| {
        matches!(event_kind(e), Some(payload::Kind::PermissionDenied(pd))
        if pd.tool_name == "shell")
    });
    assert!(
        permission_denied,
        "expected PermissionDenied event; events: {:?}",
        events
    );
    let no_string_stamp = events.iter().all(|e| {
        !matches!(event_kind(e), Some(payload::Kind::ToolResult(tr))
        if tr.output.contains("Blocked by config"))
    });
    assert!(
        no_string_stamp,
        "the \"[Blocked by config:\" string convention should be retired; events: {:?}",
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    let tx_path = crate::session::live::log_path(ws.path(), "sdeny002");
    let events = read_events(&tx_path).unwrap();

    // Same assertions as the first deny test
    let permission_denied = events.iter().any(|e| {
        matches!(event_kind(e), Some(payload::Kind::PermissionDenied(pd))
        if pd.tool_name == "shell")
    });
    assert!(
        permission_denied,
        "expected PermissionDenied event even with allow match; events: {:?}",
        events
    );
    let no_string_stamp = events.iter().all(|e| {
        !matches!(event_kind(e), Some(payload::Kind::ToolResult(tr))
        if tr.output.contains("Blocked by config"))
    });
    assert!(
        no_string_stamp,
        "the \"[Blocked by config:\" string convention should be retired; events: {:?}",
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
    let tx_path = crate::session::live::log_path(ws.path(), session_id);
    std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();

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
    wire::append(
        &tx_path,
        events::session_start(session_id.into(), None, None, false),
    )
    .unwrap();
    wire::append(&tx_path, events::user_text("run echo".into())).unwrap();
    wire::append(&tx_path, events::permissions(&[perm_item])).unwrap();
    wire::append(
        &tx_path,
        events::permissions_reply(&[pb::PermissionsReplyItem {
            id: "tc-resume-1".into(),
            decision: pb::Decision::Allow as i32,
            text: Some("shell:echo *".into()),
            scope: pb::Scope::Unspecified as i32,
        }]),
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
            initial_git: None,
            daemons: vec![],
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
async fn resume_recovers_from_crash_mid_tool_call() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let codebase = TempDir::new().unwrap();
    let session_id = "resume-crash-test";
    GitTraceRepo
        .create_session_worktree(ws.path(), session_id)
        .unwrap();
    let tx_path = crate::session::live::log_path(ws.path(), session_id);
    std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();

    // Pre-populate transcript: SessionStart + UserText + a ToolCall with NO
    // matching ToolResult — simulates gait crashing mid tool execution.
    wire::append(
        &tx_path,
        events::session_start(session_id.into(), None, None, false),
    )
    .unwrap();
    wire::append(&tx_path, events::user_text("run echo".into())).unwrap();
    wire::append(
        &tx_path,
        events::tool_call(
            "tc-crash-1".into(),
            "shell".into(),
            &serde_json::json!({"cmd": "echo hello"}),
        ),
    )
    .unwrap();

    let provider = Arc::new(FakeProvider {
        steps: Arc::new(vec![FakeStep::text("got it, retrying")]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });
    let result = run_turn_test_call(
        ws.path().to_path_buf(),
        codebase.path().to_path_buf(),
        session_id.into(),
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
            initial_git: None,
            daemons: vec![],
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
    let tool_results: Vec<_> = events
        .iter()
        .filter_map(|e| match event_kind(e) {
            Some(payload::Kind::ToolResult(tr)) => Some(tr),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_results.len(),
        1,
        "expected exactly one synthesized ToolResult for the dangling call, got: {:?}",
        tool_results
    );
    assert_eq!(tool_results[0].call_id, "tc-crash-1");
    assert!(
        tool_results[0].output.contains("interrupted"),
        "expected an interrupted/failure marker, got: {}",
        tool_results[0].output
    );

    let tool_calls = events
        .iter()
        .filter(|e| matches!(event_kind(e), Some(payload::Kind::ToolCall(_))))
        .count();
    assert_eq!(
        tool_calls, 1,
        "must not blindly re-execute — only the original ToolCall should exist"
    );
}

/// Regression test: a crash mid parallel-tool-call batch must not leave a
/// dangling `tool_use` behind. Two ToolCalls are recorded (as `execute.rs`
/// does for concurrent tool calls) but only the FIRST gets a ToolResult
/// before gait "exits" — the LAST transcript event is a ToolResult, not a
/// ToolCall, so the naive "is the last event a dangling ToolCall" check
/// would miss this entirely and hand the provider an unmatched tool_use
/// (the "tool call result does not follow tool call" HTTP 400).
#[tokio::test]
async fn resume_recovers_from_crash_mid_parallel_batch() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let codebase = TempDir::new().unwrap();
    let session_id = "resume-crash-parallel-test";
    GitTraceRepo
        .create_session_worktree(ws.path(), session_id)
        .unwrap();
    let tx_path = crate::session::live::log_path(ws.path(), session_id);
    std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();

    // Pre-populate transcript: SessionStart + UserText + two ToolCalls
    // (a parallel batch) + a ToolResult for only the FIRST call. The second
    // call ("tc-crash-2") never got a result — gait exited mid-batch.
    wire::append(
        &tx_path,
        events::session_start(session_id.into(), None, None, false),
    )
    .unwrap();
    wire::append(&tx_path, events::user_text("run two things".into())).unwrap();
    wire::append(
        &tx_path,
        events::tool_call(
            "tc-crash-1".into(),
            "read".into(),
            &serde_json::json!({"file_path": "a.txt"}),
        ),
    )
    .unwrap();
    wire::append(
        &tx_path,
        events::tool_call(
            "tc-crash-2".into(),
            "read".into(),
            &serde_json::json!({"file_path": "b.txt"}),
        ),
    )
    .unwrap();
    wire::append(
        &tx_path,
        events::tool_result("tc-crash-1".into(), "content of a".into(), None, None),
    )
    .unwrap();

    let provider = Arc::new(FakeProvider {
        steps: Arc::new(vec![FakeStep::text("got it, retrying")]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });
    let result = run_turn_test_call(
        ws.path().to_path_buf(),
        codebase.path().to_path_buf(),
        session_id.into(),
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
            initial_git: None,
            daemons: vec![],
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
    let tool_results: Vec<_> = events
        .iter()
        .filter_map(|e| match event_kind(e) {
            Some(payload::Kind::ToolResult(tr)) => Some(tr),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_results.len(),
        2,
        "expected the original result plus one synthesized ToolResult for the dangling call, got: {:?}",
        tool_results
    );
    let synthesized = tool_results
        .iter()
        .find(|tr| tr.call_id == "tc-crash-2")
        .expect("must synthesize a ToolResult for the dangling second call");
    assert!(
        synthesized.output.contains("interrupted"),
        "expected an interrupted/failure marker, got: {}",
        synthesized.output
    );

    let tool_calls = events
        .iter()
        .filter(|e| matches!(event_kind(e), Some(payload::Kind::ToolCall(_))))
        .count();
    assert_eq!(
        tool_calls, 2,
        "must not blindly re-execute — only the original two ToolCalls should exist"
    );

    // The core regression: every tool_use in the reconstructed message list
    // must have a matching tool_result, or the provider would reject the
    // message list outright.
    let messages = crate::session::retrace::messages(&events).unwrap();
    let call_ids: Vec<String> = messages
        .iter()
        .flat_map(|m| match m {
            crate::types::Message::Assistant {
                content: crate::types::AssistantContent::Parts(parts),
                ..
            } => parts
                .iter()
                .filter_map(|p| match p {
                    crate::types::AssistantPart::ToolCall(tc) => Some(tc.tool_call_id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => vec![],
        })
        .collect();
    let result_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter_map(|m| match m {
            crate::types::Message::ToolResult { content, .. } => Some(
                content
                    .iter()
                    .map(|c| c.tool_call_id.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    for id in &call_ids {
        assert!(
            result_ids.contains(id),
            "tool_use {id} has no matching tool_result in reconstructed messages: {:?}",
            messages
        );
    }
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
    let tx_path = crate::session::live::log_path(ws.path(), session_id);
    std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();

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
    wire::append(
        &tx_path,
        events::session_start(session_id.into(), None, None, false),
    )
    .unwrap();
    wire::append(&tx_path, events::user_text("clean up".into())).unwrap();
    wire::append(&tx_path, events::permissions(&[perm_item])).unwrap();
    wire::append(
        &tx_path,
        events::permissions_reply(&[pb::PermissionsReplyItem {
            id: "tc-deny-1".into(),
            decision: pb::Decision::Deny as i32,
            text: None,
            scope: pb::Scope::Unspecified as i32,
        }]),
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
            initial_git: None,
            daemons: vec![],
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
    let tx_path = crate::session::live::log_path(ws.path(), session_id);
    std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();

    // Pre-write a transcript with a Permissions+PermissionsReply already in it,
    // simulating a previous turn that was interrupted mid-permission-prompt.
    // The PermissionsReply has scope="agent" to indicate it was saved to agent config.
    let shell_args = serde_json::json!({ "cmd": "ls", "reason": "list files" });
    wire::append(
        &tx_path,
        events::session_start(session_id.into(), None, None, false),
    )
    .unwrap();
    wire::append(&tx_path, events::user_text("list files".into())).unwrap();
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
        events::permissions_reply(&[pb::PermissionsReplyItem {
            id: "tc-scope-1".into(),
            decision: pb::Decision::Allow as i32,
            text: None,
            scope: pb::Scope::Agent as i32,
        }]),
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
            initial_git: None,
            daemons: vec![],
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
async fn run_turn_persists_codebase_path_in_session_start() {
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    // After the turn, the log's SessionStart should carry codebase_path.
    use crate::session::read_session_meta;
    let meta = read_session_meta(ws.path(), session_id).unwrap();
    assert!(
        meta.codebase_path.is_some(),
        "codebase_path should be persisted in the log SessionStart"
    );
    assert_eq!(
        meta.codebase_path.as_deref(),
        Some(codebase_dir.path().to_string_lossy().as_ref()),
        "codebase_path value should match the input"
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
    let load_tool = crate::tools::LoadTool::new(vec![], vec![skill.clone()], vec![]);
    let output_tool = StructuredDoneTool::new();

    // Built via `for_test` for the tool/provider/permission wiring, then
    // `skill_registry`/`agent_whitelist` overridden directly: Phase 3's
    // catalog gate (finish_after_resume) only makes a skill visible/
    // toggle-able if it's actually present in `skill_registry`, same as the
    // live `Profile::resolve` path — the test's skill must flow through that
    // field, not just be embedded in the `LoadTool` instance, for the
    // enable() call to pass the visible-set enforcement in execute.rs.
    let caps = crate::profile::ResolvedProfile::for_test(
        provider,
        vec![Box::new(load_tool), Box::new(output_tool)],
        vec!["enable".into()],
        vec![],
    );
    let caps = crate::profile::ResolvedProfile {
        skill_registry: vec![skill],
        agent_whitelist: crate::registry::AgentWhitelist::wildcard(),
        ..caps
    };

    let session = crate::session::Session {
        id: "slogtgl01".into(),
        gait_dir: ws.path().to_path_buf(),
        codebase_path: ws.path().to_path_buf(),
        assigned_by: None,
        repos: crate::git::Repos::production(),
    };
    let (tx, _rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let emitter = Recorder::for_session(&session.gait_dir, &session.id, tx);
    run_turn(
        &session,
        &emitter,
        caps,
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        },
    )
    .await
    .unwrap();

    let tx_path = crate::session::live::log_path(ws.path(), "slogtgl01");
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

/// Phase 3 (scoped registry) default-deny, exercised end-to-end through a
/// full `run_turn`: a registered-but-not-whitelisted skill must not appear in
/// the visible set, so `enable()` on it must be a silent no-op (no `Toggle`
/// persisted) — mirrors `load_skill_emits_toggle_event` above but with the
/// default (empty) `AgentWhitelist` instead of the wildcard override.
#[tokio::test]
async fn enable_rejects_skill_outside_agent_whitelist() {
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
    let load_tool = crate::tools::LoadTool::new(vec![], vec![skill.clone()], vec![]);
    let output_tool = StructuredDoneTool::new();

    let caps = crate::profile::ResolvedProfile::for_test(
        provider,
        vec![Box::new(load_tool), Box::new(output_tool)],
        vec!["enable".into()],
        vec![],
    );
    // `agent_whitelist` deliberately left at `for_test`'s wildcard default
    // overridden down to empty (default-deny) — the one field this test
    // exercises differently from `load_skill_emits_toggle_event`.
    let caps = crate::profile::ResolvedProfile {
        skill_registry: vec![skill],
        agent_whitelist: crate::registry::AgentWhitelist::default(),
        ..caps
    };

    let session = crate::session::Session {
        id: "sdenyskil1".into(),
        gait_dir: ws.path().to_path_buf(),
        codebase_path: ws.path().to_path_buf(),
        assigned_by: None,
        repos: crate::git::Repos::production(),
    };
    let (tx, _rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let emitter = Recorder::for_session(&session.gait_dir, &session.id, tx);
    run_turn(
        &session,
        &emitter,
        caps,
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        },
    )
    .await
    .unwrap();

    let tx_path = crate::session::live::log_path(ws.path(), "sdenyskil1");
    let events = crate::proto::wire::read_events(&tx_path).unwrap();
    let toggles: Vec<_> = events
        .iter()
        .filter_map(|e| match event_kind(e) {
            Some(payload::Kind::Toggle(t)) => Some(t),
            _ => None,
        })
        .collect();
    assert!(
        toggles.is_empty(),
        "no Toggle should be persisted for a skill outside the agent's default-deny whitelist"
    );
}

/// Regression for a seed-gate leak found in review: `setup.rs`'s legacy
/// `config_enabled` union (reproducing the old flat `ActiveLoadables` seed
/// shape) re-inserted every disk-`enabled` skill into the genesis seed
/// unconditionally, bypassing the whitelist filter applied a few lines
/// above it. A disk-`enabled: true`, non-whitelisted skill must not appear
/// in the persisted `Agent` checkpoint's `skills` list, even though
/// `config_enabled` (which mirrors `Profile::resolve`'s real derivation)
/// contains it.
#[tokio::test]
async fn config_enabled_union_does_not_bypass_skill_whitelist_at_seed() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

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

    // Disk-enabled skill (mirrors a real `Profile::resolve` where an
    // `enabled: true` skill on disk feeds both `skill_registry` and
    // `config_enabled`), but not present in the agent's whitelist.
    let skill = crate::config::SkillDef {
        name: "tdd".into(),
        description: String::new(),
        content: "TDD skill content".into(),
        enabled: true,
    };
    let output_tool = StructuredDoneTool::new();
    let caps = crate::profile::ResolvedProfile::for_test(
        provider,
        vec![Box::new(output_tool)],
        vec![],
        vec![],
    );
    let caps = crate::profile::ResolvedProfile {
        skill_registry: vec![skill],
        config_enabled: std::collections::HashSet::from(["tdd".to_string()]),
        agent_whitelist: crate::registry::AgentWhitelist::default(),
        ..caps
    };

    let session = crate::session::Session {
        id: "sseedleak1".into(),
        gait_dir: ws.path().to_path_buf(),
        codebase_path: ws.path().to_path_buf(),
        assigned_by: None,
        repos: crate::git::Repos::production(),
    };
    let (tx, _rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let emitter = Recorder::for_session(&session.gait_dir, &session.id, tx);
    run_turn(
        &session,
        &emitter,
        caps,
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        },
    )
    .await
    .unwrap();

    let tx_path = crate::session::live::log_path(ws.path(), "sseedleak1");
    let events = crate::proto::wire::read_events(&tx_path).unwrap();
    let agent_checkpoint = events
        .iter()
        .rev()
        .find_map(|e| match event_kind(e) {
            Some(payload::Kind::Agent(a)) => Some(a.clone()),
            _ => None,
        })
        .expect("an Agent checkpoint must be recorded");
    assert!(
        !agent_checkpoint.skills.iter().any(|s| s == "tdd"),
        "disk-enabled but non-whitelisted skill must not leak into the genesis \
         seed via the config_enabled union: {:?}",
        agent_checkpoint.skills
    );
}

// ── Task 5: supervised mode scope pre-check ────────────────────────────────────

/// Supervised mode: write tool targeting a path outside the sandbox ceiling
/// (e.g. /etc/x, which is outside the codebase root) must return
/// TurnResult::SandboxUnlock with one item naming that path, and the file must NOT
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    // Must return TurnResult::SandboxUnlock, not Done/Permissions.
    match &result {
        TurnResult::SandboxUnlock { items } => {
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
        other => panic!("expected TurnResult::SandboxUnlock, got: {other:?}"),
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
/// `starts_with(/private/tmp/...)`, spuriously returning `TurnResult::SandboxUnlock`.
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    // Must NOT return TurnResult::SandboxUnlock — the write is inside the codebase.
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
            initial_git: None,
            daemons: vec![],
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
/// `TurnResult::SandboxUnlock` instead of continuing.
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    match &result {
        TurnResult::SandboxUnlock { items } => {
            assert!(!items.is_empty(), "expected at least one unlock item");
            let item = &items[0];
            assert_eq!(item.tool_name, "shell", "tool_name should be shell");
            assert_eq!(item.dimension, "write", "dimension should be write");
        }
        other => {
            panic!("expected TurnResult::SandboxUnlock (shell EACCES → denied), got: {other:?}")
        }
    }
}

// ── Task 4 (Phase 2d): supervised read of out-of-scope path → Unlock ─────────

/// Supervised mode: when the model issues a `read` of an absolute path that lies
/// outside the sandbox ceiling (i.e. not under the codebase root), the pre-check
/// must return `TurnResult::SandboxUnlock` with `dimension = "read"`.
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
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await
    .unwrap();

    // Cleanup sentinel file regardless of outcome.
    let _ = std::fs::remove_file(&oos_path);

    // Must return TurnResult::SandboxUnlock with dimension="read".
    match &result {
        TurnResult::SandboxUnlock { items } => {
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
        other => panic!("expected TurnResult::SandboxUnlock (read out-of-scope), got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Helpers and tests for "must call structured_done" nudge
// ---------------------------------------------------------------------------

/// Like `run_turn_test_call` but accepts a `result_schema` so we can test the
/// nudge-on-plain-text path without touching the main helper signature.
#[allow(clippy::too_many_arguments)]
async fn run_turn_with_schema(
    gait_dir: std::path::PathBuf,
    codebase_path: std::path::PathBuf,
    session_id: String,
    provider: std::sync::Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    result_schema: Option<serde_json::Value>,
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
    let caps = crate::profile::ResolvedProfile::for_test_with_schema(
        provider,
        tools,
        vec![],
        vec![],
        result_schema,
    );
    let (tx, _rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let emitter = Recorder::for_session(&session.gait_dir, &session.id, tx);
    run_turn(&session, &emitter, caps, input).await
}

/// Test (a): provider returns plain text once, then calls `structured_done`.
/// The nudge re-enters generation; the turn ultimately completes with the result.
#[tokio::test]
async fn schema_bound_plain_text_then_structured_done_succeeds() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            // Step 1: plain text (no tool calls) — triggers nudge
            vec![
                StreamChunk::Text("I will now call the tool.".into()),
                StreamChunk::Done,
            ],
            // Step 2: properly calls structured_done
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(),
                    tool_name: "structured_done".into(),
                    args: serde_json::json!({
                        "message": "done",
                        "result": { "answer": 42 }
                    }),
                },
                StreamChunk::Done,
            ],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let schema = serde_json::json!({
        "type": "object",
        "properties": { "answer": { "type": "integer" } },
        "required": ["answer"]
    });
    let output_tool = StructuredDoneTool::new();

    let result = run_turn_with_schema(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "nudge_ok_001".into(),
        provider,
        vec![output_tool.boxed()],
        Some(schema),
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: false,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await;

    assert!(
        result.is_ok(),
        "expected Ok after nudge + structured_done, got: {:?}",
        result
    );
    match result.unwrap() {
        TurnResult::Done { result, .. } => {
            assert!(result.is_some(), "expected structured result in TurnResult");
        }
        other => panic!("expected TurnResult::Done, got: {:?}", other),
    }
}

/// Test (b): provider always returns plain text, never calls `structured_done`.
/// After nudges exhausted (output_schema_retries=3) the turn should return an error.
#[tokio::test]
async fn schema_bound_plain_text_always_errors_after_nudges() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    // 4 plain-text responses (initial + 3 nudge retries) — all without tool calls
    let responses: Vec<Vec<StreamChunk>> = (0..4)
        .map(|_| {
            vec![
                StreamChunk::Text("I refuse to call the tool.".into()),
                StreamChunk::Done,
            ]
        })
        .collect();

    let provider = Arc::new(MockProvider {
        responses: Arc::new(responses),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let schema = serde_json::json!({
        "type": "object",
        "properties": { "answer": { "type": "integer" } },
        "required": ["answer"]
    });
    let output_tool = StructuredDoneTool::new();

    let result = run_turn_with_schema(
        ws.path().to_path_buf(),
        ws.path().to_path_buf(),
        "nudge_fail_001".into(),
        provider,
        vec![output_tool.boxed()],
        Some(schema),
        TurnInput {
            auto: true,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: false,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        },
        crate::git::Repos::production(),
    )
    .await;

    assert!(
        matches!(&result, Ok(crate::turn::TurnResult::Failed { ended, message })
            if ended == "error" && message.contains("structured_done")),
        "expected Ok(Failed{{error}}) after nudges exhausted, got: {result:?}"
    );
}

// ── Task 2 — run_turn primitive test ─────────────────────────────────────────
//
// The brain rewire means turn behavior is tested at the `run_turn` primitive
// (in-process, mockable). This test exercises the exact call path the rewired
// brain takes: build session + mock provider + Recorder; call run_turn directly
// (NOT run_loop); assert it returns TurnResult::Done.
#[tokio::test(flavor = "multi_thread")]
async fn run_turn_primitive_drives_one_turn_with_mock_provider() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(FakeProvider {
        steps: Arc::new(vec![FakeStep::text("Task complete.")]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let repos = crate::git::Repos::production();
    let session_id = "sprim001".to_string();
    repos
        .traces
        .create_session_worktree(ws.path(), &session_id)
        .unwrap();

    let session = crate::session::Session {
        id: session_id.clone(),
        gait_dir: ws.path().to_path_buf(),
        codebase_path: ws.path().to_path_buf(),
        assigned_by: None,
        repos,
    };

    let caps = crate::profile::ResolvedProfile::for_test(provider, vec![], vec![], vec![]);
    let (tx, _rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let emitter = crate::proto::Recorder::for_session(&session.gait_dir, &session.id, tx);

    let input = TurnInput {
        auto: true,
        max_iter: None,
        preview_policy: crate::config::PreviewPolicy::default(),
        cancel: None,
        sandbox_on: false,
        sandbox_cfg: crate::config::SandboxConfig::default(),
        relay: None,
        initial_git: None,
        daemons: vec![],
    };

    let result = run_turn(&session, &emitter, caps, input).await.unwrap();

    assert!(
        matches!(result, TurnResult::Done { .. }),
        "expected TurnResult::Done from run_turn primitive, got: {:?}",
        result
    );
}

/// Resolve the built `gait` binary for tests that spawn a real hand.
/// Prefers the cargo-injected `CARGO_BIN_EXE_gait` (only set when the test's
/// own crate owns the bin — which gait-core does NOT), else derives it from
/// this test executable's location (`target/<profile>/deps/<test>` → the
/// sibling `gait` two levels up). Returns None when no binary is present so
/// callers can skip instead of panicking.
fn gait_bin_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_gait") {
        let pb = std::path::PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let bin = exe.parent()?.parent()?.join("gait");
    bin.exists().then_some(bin)
}

/// The shell's advertised full-output path must survive the spine's per-hand
/// jail cleanup and be reachable by a later read hand through the automatic
/// current-session output grant.
#[allow(clippy::print_stderr)]
#[tokio::test]
async fn session_shell_output_is_readable_after_the_hand_exits() {
    let gait_dir = TempDir::new().unwrap();
    let codebase = TempDir::new().unwrap();
    let session_id = "sess-output-01";
    std::fs::create_dir_all(crate::session::live::worktree_dir(
        gait_dir.path(),
        session_id,
    ))
    .unwrap();

    let Some(exe) = gait_bin_path() else {
        eprintln!(
            "skipping session_shell_output_is_readable_after_the_hand_exits: `gait` binary not built"
        );
        return;
    };

    let group = crate::process::establish(None);
    let mut leader_cmd = std::process::Command::new("sleep");
    leader_cmd.arg("30");
    let mut dummy_leader = group
        .spawn_enrolled(leader_cmd)
        .expect("spawn dummy leader");

    let shell_args: pbjson_types::Struct = serde_json::from_value(serde_json::json!({
        "cmd": "printf 'later-turn-marker\\ntmp=%s\\n' \"$TMPDIR\"",
        "reason": "session output lifecycle regression"
    }))
    .unwrap();
    let shell_feedback = crate::turn::spine::run_tool_as_spine(
        gait_proto::gait::sandbox::v1::Impulse {
            tool: "shell".into(),
            args: Some(shell_args),
            requested: None,
            prefetch: None,
            shell_whitelist_patterns: vec![],
        },
        &crate::config::SandboxConfig::default(),
        gait_dir.path(),
        codebase.path(),
        session_id,
        true,
        &exe,
        group.clone(),
        None,
    )
    .await;
    let shell_result = shell_feedback.result.expect("shell feedback result");
    let shell_object = match shell_result.kind {
        Some(pbjson_types::value::Kind::StructValue(value)) => value,
        other => panic!("expected structured shell result, got {other:?}"),
    };
    let shell_output = match shell_object
        .fields
        .get("output")
        .and_then(|value| value.kind.as_ref())
    {
        Some(pbjson_types::value::Kind::StringValue(value)) => value,
        other => panic!("expected shell output string, got {other:?}"),
    };
    let path_suffix = shell_output
        .split_once("[full output: ")
        .unwrap_or_else(|| panic!("missing output path: {shell_output}"))
        .1;
    let output_path = std::path::PathBuf::from(
        path_suffix
            .split_once(" (")
            .unwrap_or_else(|| panic!("malformed output path: {shell_output}"))
            .0,
    );
    let expected_process_root = crate::session::live::outputs_dir(gait_dir.path(), session_id)
        .join("process")
        .canonicalize()
        .unwrap();
    assert!(
        shell_output.contains(&format!("tmp={}/", expected_process_root.display())),
        "spine must give shell a session-lived process TMPDIR: {shell_output}"
    );
    assert!(
        output_path.starts_with(
            crate::session::live::outputs_dir(gait_dir.path(), session_id).join("shell")
        ),
        "shell output must be session-scoped: {}",
        output_path.display()
    );
    assert!(
        output_path.exists(),
        "spine must not delete the advertised output with the hand jail"
    );

    let read_args: pbjson_types::Struct = serde_json::from_value(serde_json::json!({
        "file_path": output_path
    }))
    .unwrap();
    let read_feedback = crate::turn::spine::run_tool_as_spine(
        gait_proto::gait::sandbox::v1::Impulse {
            tool: "read".into(),
            args: Some(read_args),
            requested: None,
            prefetch: None,
            shell_whitelist_patterns: vec![],
        },
        &crate::config::SandboxConfig::default(),
        gait_dir.path(),
        codebase.path(),
        session_id,
        true,
        &exe,
        group.clone(),
        None,
    )
    .await;

    let _ = dummy_leader.kill();
    let _ = dummy_leader.wait();

    let read_result = read_feedback.result.expect("read feedback result");
    match read_result.kind {
        Some(pbjson_types::value::Kind::StringValue(value)) => assert!(
            value.contains("later-turn-marker"),
            "later read hand must reach the advertised output: {value}"
        ),
        other => panic!("expected read string result, got {other:?}"),
    }
}

/// Delegated read must serve the editor buffer, NOT disk. Seed disk with
/// different bytes to prove the buffer wins. Runs whenever the `gait` binary
/// has been built (any prior `cargo build`/`cargo test` of the workspace);
/// self-skips otherwise so a lone `cargo test -p gait-core` never fails.
// Skip diagnostic goes to stderr so the reason surfaces under `--nocapture`;
// the crate otherwise denies print_stderr per the I/O-boundary invariant.
#[allow(clippy::print_stderr)]
#[tokio::test]
async fn delegated_read_serves_buffer_not_disk() {
    use std::path::Path;
    use std::sync::Arc;
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    std::fs::create_dir_all(crate::session::live::worktree_dir(
        ws.path(),
        "sess-deleg-01",
    ))
    .unwrap();
    std::fs::write(ws.path().join("a.rs"), b"DISK CONTENT").unwrap();

    struct Fake;
    #[async_trait::async_trait]
    impl crate::embed::FsDelegate for Fake {
        async fn read_text_file(&self, _p: &Path) -> anyhow::Result<Option<String>> {
            Ok(Some("BUFFER CONTENT".into()))
        }
        async fn write_text_file(&self, _p: &Path, _c: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let Some(exe) = gait_bin_path() else {
        eprintln!(
            "skipping delegated_read_serves_buffer_not_disk: `gait` binary not built \
             (run `cargo build -p gait-bin`)"
        );
        return;
    };
    let mut s = pbjson_types::Struct::default();
    s.fields.insert(
        "file_path".into(),
        pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::StringValue("a.rs".into())),
        },
    );
    let impulse = gait_proto::gait::sandbox::v1::Impulse {
        tool: "read".into(),
        args: Some(s),
        requested: None,
        prefetch: None,
        shell_whitelist_patterns: vec![],
    };
    let group = crate::process::establish(None);
    // This test calls run_tool_as_spine directly (no real brain process), but hand
    // enrollment requires a real leader process to already be registered (the
    // hand joins the leader's OS process group via setpgid). Stand in with a
    // short-lived dummy "brain" so the group has a genuine, joinable leader.
    let mut leader_cmd = std::process::Command::new("sleep");
    leader_cmd.arg("30");
    let mut dummy_leader = group
        .spawn_enrolled(leader_cmd)
        .expect("spawn dummy leader");
    let feedback = crate::turn::spine::run_tool_as_spine(
        impulse,
        &crate::config::SandboxConfig::default(),
        ws.path(),
        ws.path(),
        "sess-deleg-01",
        false, // sandbox off keeps the test portable
        &exe,
        group,
        Some(Arc::new(Fake) as Arc<dyn crate::embed::FsDelegate>),
    )
    .await;

    let _ = dummy_leader.kill();
    let _ = dummy_leader.wait();

    let rendered = serde_json::to_string(&feedback.result).unwrap();
    assert!(
        rendered.contains("BUFFER CONTENT"),
        "delegated read must serve buffer: {rendered}"
    );
    assert!(
        !rendered.contains("DISK CONTENT"),
        "delegated read must NOT serve disk: {rendered}"
    );
}

// ─── Task 8: sandbox.shell_whitelist end-to-end integration tests ────────────────
//
// A shell command matching `sandbox.shell_whitelist` gets its `Feedback` back carrying
// `whitelist_match_items` — proves the wiring through a REAL `run_tool_as_spine`
// call and a real spawned `gait __run-tool` hand subprocess, not just the pure
// `shell_access_and_whitelist_hit` unit tests in Task 6 (which never touch the
// subprocess boundary or the real `Feedback` round-trip).
#[allow(clippy::print_stderr)]
#[tokio::test]
async fn shell_command_matching_whitelist_carries_match_items_in_feedback() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    std::fs::create_dir_all(crate::session::live::worktree_dir(
        ws.path(),
        "sess-whitelist-01",
    ))
    .unwrap();

    let Some(exe) = gait_bin_path() else {
        eprintln!(
            "skipping shell_command_matching_whitelist_carries_match_items_in_feedback: \
             `gait` binary not built (run `cargo build -p gait-bin`)"
        );
        return;
    };

    let mut s = pbjson_types::Struct::default();
    s.fields.insert(
        "cmd".into(),
        pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::StringValue(
                "flutter test --coverage".into(),
            )),
        },
    );
    let impulse = gait_proto::gait::sandbox::v1::Impulse {
        tool: "shell".into(),
        args: Some(s),
        requested: None,
        prefetch: None,
        shell_whitelist_patterns: vec![],
    };

    let sandbox_cfg = crate::config::SandboxConfig {
        shell_whitelist: vec![crate::config::SandboxWhitelistEntry {
            pattern: "flutter test *".to_string(),
            grants: vec!["network".to_string()],
        }],
        ..Default::default()
    };

    let group = crate::process::establish(None);
    let mut leader_cmd = std::process::Command::new("sleep");
    leader_cmd.arg("30");
    let mut dummy_leader = group
        .spawn_enrolled(leader_cmd)
        .expect("spawn dummy leader");

    let feedback = crate::turn::spine::run_tool_as_spine(
        impulse,
        &sandbox_cfg,
        ws.path(),
        ws.path(),
        "sess-whitelist-01",
        true, // sandbox ON
        &exe,
        group,
        None,
    )
    .await;

    let _ = dummy_leader.kill();
    let _ = dummy_leader.wait();

    assert_eq!(
        feedback.whitelist_match_items.len(),
        1,
        "matched command must carry exactly one whitelist_match_items entry (one granted dimension)"
    );
    assert_eq!(feedback.whitelist_match_items[0].dimension, "network");
    assert_eq!(feedback.whitelist_match_items[0].pattern, "flutter test *");
    assert_eq!(
        feedback.whitelist_match_items[0].resource,
        "flutter test --coverage"
    );
}

/// A non-matching shell command gets no whitelist_match_items — the negative case.
#[allow(clippy::print_stderr)]
#[tokio::test]
async fn shell_command_not_matching_whitelist_carries_no_match_items() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    std::fs::create_dir_all(crate::session::live::worktree_dir(
        ws.path(),
        "sess-whitelist-02",
    ))
    .unwrap();

    let Some(exe) = gait_bin_path() else {
        eprintln!(
            "skipping shell_command_not_matching_whitelist_carries_no_match_items: \
             `gait` binary not built (run `cargo build -p gait-bin`)"
        );
        return;
    };

    let mut s = pbjson_types::Struct::default();
    s.fields.insert(
        "cmd".into(),
        pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::StringValue("echo hi".into())),
        },
    );
    let impulse = gait_proto::gait::sandbox::v1::Impulse {
        tool: "shell".into(),
        args: Some(s),
        requested: None,
        prefetch: None,
        shell_whitelist_patterns: vec![],
    };

    let sandbox_cfg = crate::config::SandboxConfig {
        shell_whitelist: vec![crate::config::SandboxWhitelistEntry {
            pattern: "flutter test *".to_string(),
            grants: vec!["network".to_string()],
        }],
        ..Default::default()
    };

    let group = crate::process::establish(None);
    let mut leader_cmd = std::process::Command::new("sleep");
    leader_cmd.arg("30");
    let mut dummy_leader = group
        .spawn_enrolled(leader_cmd)
        .expect("spawn dummy leader");

    let feedback = crate::turn::spine::run_tool_as_spine(
        impulse,
        &sandbox_cfg,
        ws.path(),
        ws.path(),
        "sess-whitelist-02",
        true,
        &exe,
        group,
        None,
    )
    .await;

    let _ = dummy_leader.kill();
    let _ = dummy_leader.wait();

    assert!(
        feedback.whitelist_match_items.is_empty(),
        "non-matching command must not carry any whitelist_match_items, got: {:?}",
        feedback.whitelist_match_items
    );
}
