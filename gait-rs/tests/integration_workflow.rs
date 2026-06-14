use gait_ai::{
    workflow::{schema::WorkflowDef, runner::{WorkflowRunOpts, run_workflow}},
    provider::{Provider, StreamChunk, ProviderError},
    types::{Message, Tool},
};
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

struct EchoProvider;
#[async_trait::async_trait]
impl Provider for EchoProvider {
    fn model_id(&self) -> &str { "echo" }
    async fn stream(&self, _s: &str, _m: &[Message], _t: &[Box<dyn Tool>])
        -> Result<Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>, ProviderError>
    {
        Ok(Box::pin(stream! {
            yield Ok(StreamChunk::Text("done".into()));
            yield Ok(StreamChunk::ToolCallComplete {
                tool_call_id: "tc1".into(),
                tool_name: "output".into(),
                args: serde_json::json!({ "message": "shell workflow done" }),
            });
            yield Ok(StreamChunk::Done);
        }))
    }
}

#[tokio::test]
async fn shell_only_workflow_runs_and_commits() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    std::fs::create_dir_all(ws.path().join("workflows")).unwrap();

    let yaml = r#"
name: test-shell
nodes:
  - id: greet
    shell: "echo hello"
"#;
    std::fs::write(ws.path().join("workflows/test-shell.yaml"), yaml).unwrap();

    let wf: WorkflowDef = serde_yaml::from_str(yaml).unwrap();
    let session_id = "swf_test01";

    run_workflow(WorkflowRunOpts {
        workflow_def: wf,
        yaml_path: "workflows/test-shell.yaml".into(),
        session_id: session_id.into(),
        gait_dir: ws.path().to_path_buf(),
        codebase: ws.path().to_path_buf(),
        provider: Arc::new(EchoProvider),
        args: vec![],
    }).await.unwrap();

    let progress_path = ws.path().join("sessions").join(session_id).join("progress.yaml");
    assert!(progress_path.exists(), "progress.yaml must exist");
    let progress_content = std::fs::read_to_string(&progress_path).unwrap();
    assert!(progress_content.contains("done"), "greet node must be done");

    let repo = git2::Repository::open(ws.path()).unwrap();
    let branch_name = format!("session/{}", session_id);
    let branch = repo.find_branch(&branch_name, git2::BranchType::Local);
    assert!(branch.is_ok(), "session branch must exist");
}

#[tokio::test]
async fn branch_node_routes_and_skips_inactive() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let yaml = r#"
name: branch-test
nodes:
  - id: check
    shell: "echo 0"
  - id: route
    branch:
      on: "0 == 0"
      then: pass-node
      else: fail-node
    depends_on: [check]
  - id: pass-node
    shell: "echo pass"
    depends_on: [route]
  - id: fail-node
    shell: "echo fail"
    depends_on: [route]
"#;
    let wf: WorkflowDef = serde_yaml::from_str(yaml).unwrap();
    let session_id = "swf_branch01";

    run_workflow(WorkflowRunOpts {
        workflow_def: wf,
        yaml_path: "workflows/branch-test.yaml".into(),
        session_id: session_id.into(),
        gait_dir: ws.path().to_path_buf(),
        codebase: ws.path().to_path_buf(),
        provider: Arc::new(EchoProvider),
        args: vec![],
    }).await.unwrap();

    let progress_path = ws.path().join("sessions").join(session_id).join("progress.yaml");
    let content = std::fs::read_to_string(&progress_path).unwrap();
    assert!(content.contains("pass-node") && content.contains("done"), "pass-node must be done");
    assert!(content.contains("fail-node") && content.contains("skipped"), "fail-node must be skipped");
}

/// Phase 3a proof: `run_workflow` now records its events through a `Recorder` +
/// a spawned `StdioSink` consumer (no more `wire::emit`/`println!` to stdout).
/// Two guarantees:
///   1. `record` still writes the durable transcript — `workflow_start` and
///      `workflow_complete` Raw events are present in `log.jsonl`.
///   2. `.await.unwrap()` returns — the run drops the `Recorder` (closing the
///      channel) and joins the consumer cleanly, i.e. no deadlock on exit.
#[tokio::test]
async fn workflow_events_recorded_through_channel_no_stdout_echo() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    std::fs::create_dir_all(ws.path().join("workflows")).unwrap();

    let yaml = r#"
name: rec-test
nodes:
  - id: greet
    shell: "echo hi"
"#;
    std::fs::write(ws.path().join("workflows/rec-test.yaml"), yaml).unwrap();
    let wf: WorkflowDef = serde_yaml::from_str(yaml).unwrap();
    let session_id = "swf_rec01";

    run_workflow(WorkflowRunOpts {
        workflow_def: wf,
        yaml_path: "workflows/rec-test.yaml".into(),
        session_id: session_id.into(),
        gait_dir: ws.path().to_path_buf(),
        codebase: ws.path().to_path_buf(),
        provider: Arc::new(EchoProvider),
        args: vec![],
    }).await.unwrap();

    let log = ws.path().join("sessions").join(session_id).join("log.jsonl");
    let content = std::fs::read_to_string(&log).unwrap();
    assert!(content.contains("\"type\":\"workflow_start\""), "log must contain workflow_start: {content}");
    assert!(content.contains("\"type\":\"workflow_complete\""), "log must contain workflow_complete: {content}");
}

/// Phase 3b proof: a prompt node's sub-turn must publish its events to the
/// WORKFLOW's channel (the dropped-receiver fix), not into a discarded receiver.
/// We drive `exec_prompt_node` directly with a test-owned sender and assert at
/// least one sub-turn event arrives on it. A dummy api key lets the provider
/// construct; `EchoProvider` overrides it, so no network call is made.
#[tokio::test]
async fn prompt_subturn_publishes_to_workflow_channel() {
    use gait_ai::workflow::exec_shell::NodeContext;
    use gait_ai::workflow::exec_prompt::exec_prompt_node;
    use gait_ai::workflow::vars::VarContext;
    use gait_ai::proto::gait::log::v1::{StreamItem, stream_item, event, payload};

    std::env::set_var("MINIMAX_API_KEY", "test-key-not-used");

    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let yaml = "name: prompt-test\nnodes:\n  - id: p\n    prompt: \"say hi\"\n";
    let wf: WorkflowDef = serde_yaml::from_str(yaml).unwrap();
    let node = &wf.nodes[0];
    let session_id = "swf_prompt01";

    // exec_prompt_node now pre-appends the prompt as a user_turn (Phase 4 Option A),
    // so the session worktree must exist (run_workflow creates it in production; here
    // we drive exec_prompt_node directly).
    gait_ai::git::Repos::production().traces.create_session_worktree(ws.path(), session_id).unwrap();

    // Test owns the channel; assert the sub-turn published to THIS sender. No
    // spawned consumer here, so there is no consumer.await and no deadlock risk.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();

    {
        let ctx = NodeContext {
            workflow_id: "wf1",
            session_id,
            gait_dir: ws.path(),
            codebase: ws.path(),
            workflow_tx: &tx,
            vars: VarContext {
                workflow_id: "wf1".into(), session_id: session_id.into(),
                gait_dir: ws.path().to_path_buf(), codebase: ws.path().to_path_buf(),
                args: "".into(), node_id: "p".into(),
                node_outputs: Default::default(), loop_iter: None, loop_max: None,
                loop_output: None, loop_signal: None, node_exit: None, failed_node: None,
            },
        };
        exec_prompt_node(node, "say hi", &ctx, &wf, Arc::new(EchoProvider)).await.unwrap();
    } // ctx drops here, releasing the &tx borrow

    let mut saw_event = false;
    while let Ok(item) = rx.try_recv() {
        if let Some(stream_item::Item::Event(ev)) = item.item {
            if let Some(event::Body::Payload(p)) = ev.body {
                if matches!(p.kind,
                    Some(payload::Kind::UserTurn(_))
                    | Some(payload::Kind::AssistantText(_))
                    | Some(payload::Kind::ToolCall(_))) {
                    saw_event = true;
                }
            }
        }
    }
    assert!(saw_event, "prompt sub-turn must publish events to the workflow channel (3b)");
}
