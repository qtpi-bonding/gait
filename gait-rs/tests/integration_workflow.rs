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
