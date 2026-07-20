use crate::config::load_config;
use crate::git::{build_trace_message, TraceTrailers};
use crate::proto::{events, wire};
use crate::session::log::{init_session, InitSessionOpts};
use crate::tools::ToolError;
use crate::tools::{make_schema, ToolCtx};
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// Options for the shared child-session disk-write step.
pub struct WriteChildSessionOpts<'a> {
    pub gait_dir: &'a std::path::Path,
    pub codebase: &'a std::path::Path,
    pub new_id: &'a str,
    pub parent_session_id: &'a str,
    pub task: String,
    pub notify_done: bool,
    /// Per-spawn named inference, if requested by the parent.
    pub inference: Option<String>,
    pub repos: &'a crate::git::Repos,
}

/// Shared logic: create session worktree, init session, seed task message, commit.
/// Used by both `SpawnAgentTool::execute` (in-process / test path) and
/// `handle_spawn_agent` (spine relay path) to avoid duplicating disk-write logic.
pub fn write_child_session(opts: WriteChildSessionOpts<'_>) -> anyhow::Result<()> {
    opts.repos
        .traces
        .create_session_worktree(opts.gait_dir, opts.new_id)?;

    let config = load_config(opts.gait_dir);
    let codebase_hash = opts
        .repos
        .codebase
        .head_hash(opts.codebase)
        .unwrap_or_default();
    let tx_path = crate::session::live::log_path(opts.gait_dir, opts.new_id);
    // The nested sessions/<id>/ dir doesn't exist on a fresh session — `gait init`
    // creates .active/ but not its children. Ensure the log's parent dir exists
    // before init_session and wire::append try to open it.
    if let Some(parent) = tx_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let inference = crate::profile::resolve_inference_checked(
        opts.gait_dir,
        &config,
        None,
        opts.inference.as_deref(),
    )?;
    let session_model = inference.model.clone();
    init_session(InitSessionOpts {
        log_path: tx_path.clone(),
        id: opts.new_id.to_string(),
        model: session_model,
        codebase_hash: codebase_hash.clone(),
        codebase_dirty: false,
        assigned_by: Some(opts.parent_session_id.to_string()),
        codebase_path: Some(opts.codebase.to_string_lossy().into_owned()),
        notify_done: opts.notify_done,
    })?;
    wire::append(&tx_path, events::inference(inference))?;

    let from = format!("agent:{}", opts.parent_session_id);
    wire::append(&tx_path, events::user_text(opts.task, Some(from)))?;

    opts.repos.traces.commit_session(
        opts.gait_dir,
        opts.new_id,
        &build_trace_message(
            &format!(
                "session({}): spawn from {}",
                opts.new_id, opts.parent_session_id
            ),
            &TraceTrailers {
                phase: "spawn".to_string(),
                parent_session: Some(opts.parent_session_id.to_string()),
                codebase: opts.codebase.display().to_string(),
                codebase_hash: codebase_hash.clone(),
                ..Default::default()
            },
        ),
    )?;

    Ok(())
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
struct SpawnAgentArgs {
    /// Task description — injected as the first user_turn in the new session
    task: String,
    /// Explicit named inference for the child.
    inference: Option<String>,
    /// Optional JSON schema the child's structured_done result must conform to.
    /// Only meaningful when detached=false (blocking subagent).
    schema: Option<serde_json::Value>,
    /// Optional files to prepend as context (reserved for future use)
    context_files: Option<Vec<String>>,
    /// When true, spawn an independent peer agent that outlives this one.
    /// When false (default), spawn a subagent that is cascade-killed when this agent ends.
    #[serde(default)]
    detached: bool,
}
#[allow(dead_code)]
#[derive(schemars::JsonSchema)]
struct SpawnAgentOutput {
    message: String,
    result: Option<serde_json::Value>,
}

pub struct SpawnAgentTool {
    ctx: ToolCtx,
    repos: crate::git::Repos,
    parameters: Value,
    output_schema: Value,
}

impl SpawnAgentTool {
    pub fn new(ctx: ToolCtx, repos: crate::git::Repos) -> Self {
        Self {
            ctx,
            repos,
            parameters: crate::tools::inject_filter(
                make_schema::<SpawnAgentArgs>(),
                crate::types::OutputFilter::Both,
            ),
            output_schema: crate::tools::make_schema::<SpawnAgentOutput>(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }
    fn description(&self) -> &str {
        "Delegate a subtask to a subagent. By default (detached:false) this blocks until the child finishes and returns its structured_done result; pass a schema to require a typed result. detached:true returns {session_id} immediately (peer agent)."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&self.output_schema)
    }

    async fn execute(&self, args: Value, rec: &dyn crate::types::Rec) -> Result<Value, ToolError> {
        let task = args["task"]
            .as_str()
            .ok_or_else(|| {
                crate::tools::ToolError::Execute("spawn_agent: missing task".to_string())
            })?
            .to_string();

        let new_id = crate::session::new_session_id();
        let is_detached = args["detached"].as_bool().unwrap_or(false);
        let inference = args
            .get("inference")
            .and_then(Value::as_str)
            .map(str::to_owned);

        write_child_session(WriteChildSessionOpts {
            gait_dir: &self.ctx.gait_dir,
            codebase: &self.ctx.codebase_path,
            new_id: &new_id,
            parent_session_id: &self.ctx.session_id,
            task,
            notify_done: is_detached,
            inference: inference.clone(),
            repos: &self.repos,
        })
        .map_err(crate::tools::ToolError::Internal)?;

        // Persist per-spawn schema so the child CLI can read it into ProfileInputs.
        if let Some(schema) = args.get("schema").filter(|v| !v.is_null()) {
            let schema_path = crate::session::live::worktree_dir(&self.ctx.gait_dir, &new_id)
                .join("result_schema.json");
            match serde_json::to_vec_pretty(schema) {
                Ok(bytes) => {
                    if let Err(e) = std::fs::write(&schema_path, bytes) {
                        eprintln!("[spawn_agent] warning: failed to write result_schema.json: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("[spawn_agent] warning: failed to serialize schema: {e}");
                }
            }
        }

        // Schedule child for execution — via Spawn signal if in brain (sealed) mode,
        // or direct Command::spawn otherwise. Spine appends --sandbox-locked when
        // sandbox is active so the child cannot disable sandboxing.
        // detached=false → subagent (cascade-killed with parent); detached=true → peer (survives).
        let argv = crate::launch::RunLaunch {
            session: Some(new_id.clone()),
            gait_dir: Some(self.ctx.gait_dir.clone()),
            auto: true,
            inference,
            ..Default::default()
        }
        .to_process_argv();
        rec.spawn_process(argv, is_detached);

        Ok(serde_json::json!({ "session_id": new_id }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::gait::log::v1::{event, payload};
    use crate::session::log::read_events;
    use crate::session::read_session_meta;
    use crate::test_utils::init_git_repo;
    use tempfile::TempDir;

    #[tokio::test]
    async fn spawn_agent_creates_session_transcript() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        std::fs::create_dir_all(ws.path().join("inferences/parent")).unwrap();
        std::fs::write(
            ws.path().join("inferences/parent/INFERENCE.yaml"),
            "model: minimax/MiniMax-M3\n",
        )
        .unwrap();
        std::fs::create_dir_all(ws.path().join("inferences/child")).unwrap();
        std::fs::write(
            ws.path().join("inferences/child/INFERENCE.yaml"),
            "model: anthropic/child\n",
        )
        .unwrap();
        std::fs::write(
            ws.path().join("gait.jsonc"),
            r#"{"inference":"parent","allow":[],"tools":[]}"#,
        )
        .unwrap();

        let tool = SpawnAgentTool::new(
            ToolCtx::new(
                ws.path().to_path_buf(),
                ws.path().to_path_buf(),
                "scaller001".into(),
            ),
            crate::git::Repos::production(),
        );
        assert!(tool.parameters()["properties"]["inference"].is_object());

        let result = tool
            .execute(
                serde_json::json!({
                    "task": "implement feature X",
                    "inference": "child"
                }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();

        let new_id = result["session_id"].as_str().unwrap();
        assert!(new_id.starts_with('s'));

        let tx = crate::session::live::log_path(ws.path(), new_id);
        assert!(tx.exists(), "child session log must exist at {tx:?}");
        let events = read_events(&tx).unwrap();
        let ek = |e: &crate::proto::gait::log::v1::Event| -> Option<payload::Kind> {
            match &e.body {
                Some(event::Body::Payload(p)) => p.kind.clone(),
                _ => None,
            }
        };
        let types: Vec<&str> = events
            .iter()
            .map(|e| match ek(e) {
                Some(payload::Kind::SessionStart(_)) => "session_start",
                Some(payload::Kind::UserText(_)) => "user_turn",
                _ => "other",
            })
            .collect();
        assert!(types.contains(&"session_start"), "{:?}", types);
        assert!(types.contains(&"user_turn"), "{:?}", types);

        let user_turn = events
            .iter()
            .find_map(|e| match ek(e) {
                Some(payload::Kind::UserText(u)) => Some(u),
                _ => None,
            })
            .unwrap();
        assert_eq!(user_turn.content, "implement feature X");
        assert_eq!(user_turn.from.as_deref(), Some("agent:scaller001"));

        let meta = read_session_meta(ws.path(), new_id).unwrap();
        assert_eq!(meta.assigned_by.as_deref(), Some("scaller001"));
    }

    #[tokio::test]
    async fn spawn_agent_returns_session_id() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        std::fs::create_dir_all(ws.path().join("inferences/parent")).unwrap();
        std::fs::write(
            ws.path().join("inferences/parent/INFERENCE.yaml"),
            "model: minimax/MiniMax-M3\n",
        )
        .unwrap();
        std::fs::write(
            ws.path().join("gait.jsonc"),
            r#"{"inference":"parent","allow":[],"tools":[]}"#,
        )
        .unwrap();

        let tool = SpawnAgentTool::new(
            ToolCtx::new(
                ws.path().to_path_buf(),
                ws.path().to_path_buf(),
                "scaller".into(),
            ),
            crate::git::Repos::production(),
        );
        let result = tool
            .execute(
                serde_json::json!({ "task": "foo" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let id = result["session_id"].as_str().unwrap();
        assert_eq!(id.len(), 9);
    }

    #[tokio::test]
    async fn spawn_agent_commit_has_parent_session_trailer() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        std::fs::create_dir_all(ws.path().join("inferences/parent")).unwrap();
        std::fs::write(
            ws.path().join("inferences/parent/INFERENCE.yaml"),
            "model: minimax/MiniMax-M3\n",
        )
        .unwrap();
        std::fs::write(
            ws.path().join("gait.jsonc"),
            r#"{"inference":"parent","allow":[],"tools":[]}"#,
        )
        .unwrap();

        let tool = SpawnAgentTool::new(
            ToolCtx::new(
                ws.path().to_path_buf(),
                ws.path().to_path_buf(),
                "sparent01".into(),
            ),
            crate::git::Repos::production(),
        );

        let result = tool
            .execute(
                serde_json::json!({"task": "do the thing"}),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let new_id = result["session_id"].as_str().unwrap().to_string();

        let child_dir = crate::session::live::worktree_dir(ws.path(), &new_id);
        let repo = git2::Repository::open(&child_dir).unwrap();
        let msg = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string();
        assert!(
            msg.contains("Phase: spawn"),
            "expected Phase: spawn trailer, got: {msg}"
        );
        assert!(
            msg.contains("Parent-Session: sparent01"),
            "expected Parent-Session: sparent01 trailer, got: {msg}"
        );
    }
}
