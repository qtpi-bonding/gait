use crate::config::load_config;
use crate::proto::{events, wire};
use crate::session::log::{init_session, InitSessionOpts};
use crate::tools::ToolError;
use crate::tools::{make_schema, ToolCtx};
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
struct SpawnAgentArgs {
    /// Task description — injected as the first user_turn in the new session
    task: String,
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
        "Delegate a subtask to a new agent session. Returns {session_id} immediately. detached: spawn an independent peer agent that outlives this one (default false = subagent that ends with this agent). The child runs on the same codebase using the gait.jsonc model and allow/deny, starts a fresh log, and runs --auto. Use list_agents to check status."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&self.output_schema)
    }

    async fn execute(&self, args: Value, rec: &crate::Recorder) -> Result<Value, ToolError> {
        let task = args["task"]
            .as_str()
            .ok_or_else(|| {
                crate::tools::ToolError::Execute("spawn_agent: missing task".to_string())
            })?
            .to_string();

        let new_id = crate::session::new_session_id();

        self.repos
            .traces
            .create_session_worktree(&self.ctx.gait_dir, &new_id)
            .map_err(crate::tools::ToolError::Internal)?;

        let config = load_config(&self.ctx.gait_dir);
        let codebase_hash = self
            .repos
            .codebase
            .head_hash(&self.ctx.codebase_path)
            .unwrap_or_default();
        let tx_path = self
            .ctx
            .gait_dir
            .join("sessions")
            .join(&new_id)
            .join("log.jsonl");

        init_session(InitSessionOpts {
            log_path: tx_path.clone(),
            id: new_id.clone(),
            model: config.model,
            codebase_hash,
            codebase_dirty: false,
            assigned_by: Some(self.ctx.session_id.clone()),
            codebase_path: Some(self.ctx.codebase_path.to_string_lossy().to_string()),
        })
        .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;

        // Append the initial user message (task) so the child session has input to fold.
        let from = format!("agent:{}", self.ctx.session_id);
        wire::append(&tx_path, events::user_text(task, Some(from)))
            .map_err(crate::tools::ToolError::Internal)?;

        self.repos
            .traces
            .commit_session(
                &self.ctx.gait_dir,
                &new_id,
                &format!("session({}): spawn from {}", new_id, self.ctx.session_id),
            )
            .map_err(crate::tools::ToolError::Internal)?;

        let is_detached = args["detached"].as_bool().unwrap_or(false);

        // Schedule child for execution — via Spawn signal if in brain (sealed) mode,
        // or direct Command::spawn otherwise. Spine appends --sandbox-locked when
        // sandbox is active so the child cannot disable sandboxing.
        // detached=false → subagent (cascade-killed with parent); detached=true → peer (survives).
        rec.spawn_process(
            vec![
                "run".to_string(),
                "--session".to_string(),
                new_id.clone(),
                "--gait-dir".to_string(),
                self.ctx.gait_dir.to_str().unwrap_or(".").to_string(),
                "--auto".to_string(),
            ],
            is_detached,
        );

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
        std::fs::write(
            ws.path().join("gait.jsonc"),
            r#"{"model":"minimax/MiniMax-M3","allow":[],"tools":[]}"#,
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

        let result = tool
            .execute(
                serde_json::json!({
                    "task": "implement feature X"
                }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();

        let new_id = result["session_id"].as_str().unwrap();
        assert!(new_id.starts_with('s'));

        let tx = ws.path().join(format!("sessions/{}/log.jsonl", new_id));
        assert!(tx.exists());
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
        std::fs::write(
            ws.path().join("gait.jsonc"),
            r#"{"model":"minimax/MiniMax-M3","allow":[],"tools":[]}"#,
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
}
