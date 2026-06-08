use std::path::PathBuf;
use serde_json::Value;
use schemars::JsonSchema;
use serde::Deserialize;
use crate::types::Tool;
use crate::session::transcript::{AnyEvent, InitSessionOpts, append_event, init_session, now_ms};
use crate::session::session_json::{SessionJson, TokenTotals, write_session_json};
use crate::git;
use crate::config::load_config;
use crate::tools::make_schema;

#[derive(Deserialize, JsonSchema)]
struct SpawnAgentArgs {
    /// Task description — injected as the first user_turn in the new session
    task: String,
    /// Optional files to prepend as context (reserved for future use)
    context_files: Option<Vec<String>>,
}

pub struct SpawnAgentTool {
    current_session_id: String,
    workspace: PathBuf,
    codebase_path: PathBuf,
    parameters: Value,
}

impl SpawnAgentTool {
    pub fn new(current_session_id: String, workspace: PathBuf, codebase_path: PathBuf) -> Self {
        Self {
            current_session_id,
            workspace,
            codebase_path,
            parameters: make_schema::<SpawnAgentArgs>(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &str { "spawn_agent" }
    fn description(&self) -> &str {
        "Spawn a new agent session for a subtask. Returns the session ID immediately without blocking. The harness will schedule the new session for execution."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let task = args["task"].as_str()
            .ok_or_else(|| anyhow::anyhow!("spawn_agent: missing task"))?
            .to_string();

        let new_id = crate::chat::new_session_id();

        git::create_session_worktree(&self.workspace, &new_id)?;

        let sj = SessionJson {
            id: new_id.clone(),
            assigned_by: Some(self.current_session_id.clone()),
            created_at: now_ms(),
            token_totals: TokenTotals::default(),
            codebase_path: Some(self.codebase_path.to_string_lossy().to_string()),
            agent_name: None,
            system_prompt: None,
        };
        write_session_json(&self.workspace, &sj)?;

        let config = load_config(&self.workspace);
        let codebase_hash = git::get_codebase_hash(&self.codebase_path);
        let tx_path = self.workspace.join("sessions").join(&new_id).join("transcript.jsonl");

        init_session(InitSessionOpts {
            transcript_path: tx_path.clone(),
            id: new_id.clone(),
            model: config.model,
            codebase_hash,
            codebase_dirty: false,
        })?;

        append_event(&tx_path, &AnyEvent::UserTurn {
            content: task,
            from: Some(format!("agent:{}", self.current_session_id)),
            context: None,
            ts: now_ms(),
        })?;

        git::commit_session(
            &self.workspace,
            &new_id,
            &format!("session({}): spawn from {}", new_id, self.current_session_id),
        )?;

        // I1 fix: immediately schedule the child for execution (detached, fire-and-forget).
        let exe = std::env::current_exe()
            .unwrap_or_else(|_| std::path::PathBuf::from("gait"));
        let _ = std::process::Command::new(&exe)
            .args([
                "run",
                "--session", &new_id,
                "--workspace", self.workspace.to_str().unwrap_or("."),
                "--auto",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn(); // best-effort; spawn failure is non-fatal

        Ok(serde_json::json!({ "session_id": new_id }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use crate::session::transcript::{read_events, AnyEvent};
    use crate::session::session_json::read_session_json;

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

    #[tokio::test]
    async fn spawn_agent_creates_session_transcript() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        std::fs::write(ws.path().join("gait.jsonc"),
            r#"{"model":"minimax/MiniMax-M1-80k","allow":[],"tools":[]}"#).unwrap();

        let tool = SpawnAgentTool::new(
            "scaller001".into(),
            ws.path().to_path_buf(),
            ws.path().to_path_buf(),
        );

        let result = tool.execute(serde_json::json!({
            "task": "implement feature X"
        })).await.unwrap();

        let new_id = result["session_id"].as_str().unwrap();
        assert!(new_id.starts_with('s'));

        let tx = ws.path().join(format!("sessions/{}/transcript.jsonl", new_id));
        assert!(tx.exists());
        let events = read_events(&tx).unwrap();
        let types: Vec<&str> = events.iter().map(|e| match e {
            AnyEvent::SessionStart { .. } => "session_start",
            AnyEvent::UserTurn { .. } => "user_turn",
            _ => "other",
        }).collect();
        assert!(types.contains(&"session_start"), "{:?}", types);
        assert!(types.contains(&"user_turn"), "{:?}", types);

        let user_turn = events.iter().find(|e| matches!(e, AnyEvent::UserTurn { .. })).unwrap();
        if let AnyEvent::UserTurn { content, from, .. } = user_turn {
            assert_eq!(content, "implement feature X");
            assert_eq!(from.as_deref(), Some("agent:scaller001"));
        }

        let sj = read_session_json(ws.path(), new_id);
        assert_eq!(sj.assigned_by.as_deref(), Some("scaller001"));
    }

    #[tokio::test]
    async fn spawn_agent_returns_session_id() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        std::fs::write(ws.path().join("gait.jsonc"),
            r#"{"model":"minimax/MiniMax-M1-80k","allow":[],"tools":[]}"#).unwrap();

        let tool = SpawnAgentTool::new("scaller".into(), ws.path().to_path_buf(), ws.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "task": "foo" })).await.unwrap();
        let id = result["session_id"].as_str().unwrap();
        assert_eq!(id.len(), 9);
    }
}
