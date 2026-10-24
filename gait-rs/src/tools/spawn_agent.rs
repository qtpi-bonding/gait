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
    /// The spawning agent's identity; signs the child's task. `None` is a hard error.
    pub identity: Option<&'a gait_identity::IdentityContext>,
}

pub(crate) fn wall_clock_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Shared logic: create session worktree, init session, seed task message, commit.
/// Used by both `SpawnAgentTool::execute` (in-process / test path) and
/// `handle_spawn_agent` (spine relay path) to avoid duplicating disk-write logic.
pub fn write_child_session(opts: WriteChildSessionOpts<'_>) -> anyhow::Result<()> {
    let identity = opts.identity.ok_or_else(|| {
        anyhow::anyhow!(
            "spawn_agent: no agent identity configured (GAIT_AGENT_DIR unset); cannot sign the child's task"
        )
    })?;
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

    wire::append_with_signed(
        &tx_path,
        events::user_text(opts.task),
        crate::proto::envelope::EventScope::Session,
        |ev| {
            gait_identity::sign_direct_event(identity, ev, opts.new_id, 3600, &wall_clock_secs)
        },
    )?;

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
        "Delegate a subtask to another agent. The default waits for the child and returns its result. Use detached=true to start a peer and return its session_id immediately."
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
            identity: self.ctx.identity.as_deref(),
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
    use gait_identity::{DirectVerifier, IdentityContext, ProvenanceVerifier};
    use identity_keyfile::testkit::TempAgentDir;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// A real agent identity minted now, with a one-hour device window, so a
    /// child event stamped at wall-clock time verifies. Keep the fixture alive
    /// while the identity is in use.
    fn agent_identity() -> (TempAgentDir, Arc<IdentityContext>) {
        let now = wall_clock_secs();
        let agent = TempAgentDir::new(now, 3_600);
        let identity = Arc::new(IdentityContext::new(agent.path()).unwrap());
        (agent, identity)
    }

    #[tokio::test]
    async fn spawn_agent_creates_session_transcript() {
        let ws = TempDir::new().unwrap();
        let (_agent, identity) = agent_identity();
        init_git_repo(ws.path());
        std::fs::create_dir_all(ws.path().join("inferences/parent")).unwrap();
        std::fs::write(
            ws.path().join("inferences/parent/inference.textproto"),
            "model: \"minimax/MiniMax-M3\"\ncompaction_threshold_tokens: 128000\n",
        )
        .unwrap();
        std::fs::create_dir_all(ws.path().join("inferences/child")).unwrap();
        std::fs::write(
            ws.path().join("inferences/child/inference.textproto"),
            "model: \"anthropic/child\"\ncompaction_threshold_tokens: 128000\n",
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
            ).with_identity(Some(identity.clone())),
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

        let user_turn_event = events
            .iter()
            .find(|e| matches!(ek(e), Some(payload::Kind::UserText(_))))
            .unwrap();
        let user_turn = match ek(user_turn_event) {
            Some(payload::Kind::UserText(u)) => u,
            _ => unreachable!(),
        };
        assert_eq!(user_turn.content, "implement feature X");
        let prov = user_turn_event
            .provenance
            .first()
            .expect("user_turn must carry provenance");
        assert!(
            prov.signed_message.is_some(),
            "spawn_agent must sign the task with a direct signature"
        );
        let verifier = DirectVerifier::from_context(&identity).unwrap();
        assert_eq!(
            verifier.verify_direct(user_turn_event, new_id),
            Some(identity.agent_id().clone()),
            "spawn_agent must sign with a real, verifiable signature for the child session"
        );

        let meta = read_session_meta(ws.path(), new_id).unwrap();
        assert_eq!(meta.assigned_by.as_deref(), Some("scaller001"));
    }

    #[tokio::test]
    async fn spawn_agent_without_identity_is_a_hard_error() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        std::fs::create_dir_all(ws.path().join("inferences/parent")).unwrap();
        std::fs::write(
            ws.path().join("inferences/parent/inference.textproto"),
            "model: \"minimax/MiniMax-M3\"\ncompaction_threshold_tokens: 128000\n",
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
                "sparent03".into(),
            ),
            crate::git::Repos::production(),
        );
        let err = tool
            .execute(
                serde_json::json!({ "task": "foo" }),
                &crate::Recorder::detached(),
            )
            .await
            .expect_err("spawn_agent must not write an unsigned task");

        assert!(
            format!("{err:?}").contains("no agent identity configured"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn spawn_agent_returns_session_id() {
        let ws = TempDir::new().unwrap();
        let (_agent, identity) = agent_identity();
        init_git_repo(ws.path());
        std::fs::create_dir_all(ws.path().join("inferences/parent")).unwrap();
        std::fs::write(
            ws.path().join("inferences/parent/inference.textproto"),
            "model: \"minimax/MiniMax-M3\"\ncompaction_threshold_tokens: 128000\n",
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
            ).with_identity(Some(identity.clone())),
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
        let (_agent, identity) = agent_identity();
        init_git_repo(ws.path());
        std::fs::create_dir_all(ws.path().join("inferences/parent")).unwrap();
        std::fs::write(
            ws.path().join("inferences/parent/inference.textproto"),
            "model: \"minimax/MiniMax-M3\"\ncompaction_threshold_tokens: 128000\n",
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
            ).with_identity(Some(identity.clone())),
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

    #[tokio::test]
    async fn spawn_agent_task_injection_carries_provenance() {
        let ws = TempDir::new().unwrap();
        let (_agent, identity) = agent_identity();
        init_git_repo(ws.path());
        std::fs::create_dir_all(ws.path().join("inferences/parent")).unwrap();
        std::fs::write(
            ws.path().join("inferences/parent/inference.textproto"),
            "model: \"minimax/MiniMax-M3\"\ncompaction_threshold_tokens: 128000\n",
        )
        .unwrap();
        std::fs::create_dir_all(ws.path().join("inferences/child")).unwrap();
        std::fs::write(
            ws.path().join("inferences/child/inference.textproto"),
            "model: \"anthropic/child\"\ncompaction_threshold_tokens: 128000\n",
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
            ).with_identity(Some(identity.clone())),
            crate::git::Repos::production(),
        );

        let result = tool
            .execute(
                serde_json::json!({"task": "implement feature X", "inference": "child"}),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let new_id = result["session_id"].as_str().unwrap();

        let tx = crate::session::live::log_path(ws.path(), new_id);
        let events = read_events(&tx).unwrap();
        let ek = |e: &crate::proto::gait::log::v1::Event| -> Option<payload::Kind> {
            match &e.body {
                Some(event::Body::Payload(p)) => p.kind.clone(),
                _ => None,
            }
        };
        let user_text_event = events
            .iter()
            .find(|e| matches!(ek(e), Some(payload::Kind::UserText(_))))
            .expect("task injection event must exist");
        assert!(
            user_text_event
                .provenance
                .first()
                .is_some_and(|p| p.signed_message.is_some()),
            "task injection must carry a direct signature"
        );
        let verifier = DirectVerifier::from_context(&identity).unwrap();
        assert_eq!(
            verifier.verify_direct(user_text_event, new_id),
            Some(identity.agent_id().clone()),
            "spawn_agent must sign with a real, verifiable signature for the child session"
        );
    }
}
