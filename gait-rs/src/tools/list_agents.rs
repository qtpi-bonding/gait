use std::path::PathBuf;
use serde_json::Value;
use schemars::JsonSchema;
use serde::Deserialize;
use crate::types::{Tool, ExecutionMode};
use crate::session::transcript::AnyEvent;

#[derive(Deserialize, JsonSchema)]
struct ListAgentsParams {}

pub struct ListAgentsTool {
    workspace: PathBuf,
    parameters: Value,
}

impl ListAgentsTool {
    pub fn new(workspace: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<ListAgentsParams>();
        Self {
            workspace,
            parameters,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ListAgentsTool {
    fn name(&self) -> &str { "list_agents" }
    fn description(&self) -> &str {
        "List active agent sessions not yet done, sorted by last activity descending."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }

    async fn execute(&self, _args: Value) -> anyhow::Result<Value> {
        // C1 fix: read transcript from the branch's committed git tree, NOT from the filesystem
        // working copy. Worktree dirs may be cleaned up after finish_session; the branch remains.
        let repo = match git2::Repository::open(&self.workspace) {
            Ok(r) => r,
            Err(_) => return Ok(serde_json::json!([])),
        };

        let mut results: Vec<serde_json::Value> = vec![];

        let branches = match repo.branches(Some(git2::BranchType::Local)) {
            Ok(b) => b,
            Err(_) => return Ok(serde_json::json!([])),
        };

        for branch_result in branches {
            let (branch, _) = match branch_result { Ok(b) => b, Err(_) => continue };
            let name = match branch.name() {
                Ok(Some(n)) => n.to_string(),
                _ => continue,
            };
            let session_id = match name.strip_prefix("session/") {
                Some(id) => id.to_string(),
                None => continue,
            };

            // Read transcript.jsonl blob from the branch's committed tree
            let reference = branch.into_reference();
            let commit = match reference.peel_to_commit() {
                Ok(c) => c,
                Err(_) => continue,
            };
            let tree = match commit.tree() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let blob_content = match tree.get_name("transcript.jsonl") {
                Some(entry) => match repo.find_blob(entry.id()) {
                    Ok(blob) => match std::str::from_utf8(blob.content()) {
                        Ok(s) => s.to_string(),
                        Err(_) => continue,
                    },
                    Err(_) => continue,
                },
                None => continue,
            };

            let events: Vec<AnyEvent> = blob_content.lines()
                .filter(|l| !l.is_empty())
                .filter_map(|l| serde_json::from_str::<AnyEvent>(l).ok())
                .collect();
            if events.is_empty() { continue; }

            let last_evt = events.last().unwrap();
            let ts = match last_evt {
                AnyEvent::SessionStart { ts, .. } => *ts,
                AnyEvent::UserTurn { ts, .. } => *ts,
                AnyEvent::AssistantText { ts, .. } => *ts,
                AnyEvent::ToolCall { ts, .. } => *ts,
                AnyEvent::ToolResult { ts, .. } => *ts,
                AnyEvent::TurnComplete { ts, .. } => *ts,
                AnyEvent::Error { ts, .. } => *ts,
                AnyEvent::Permissions { ts, .. } => *ts,
                AnyEvent::AgentUpdate { ts, .. } => *ts,
                _ => 0,
            };

            let status = match last_evt {
                AnyEvent::TurnComplete { .. } => "done",
                AnyEvent::Error { .. } => "error",
                AnyEvent::Clarify { .. } => "clarify",
                AnyEvent::Inform { .. } => "inform",
                AnyEvent::Permissions { .. } => "permissions",
                _ => "running",
            };

            if status == "done" { continue; }

            let summary = events.iter().rev()
                .find_map(|e| if let AnyEvent::AssistantText { content, .. } = e {
                    Some(content.chars().take(100).collect::<String>())
                } else {
                    None
                })
                .unwrap_or_default();

            results.push(serde_json::json!({
                "session_id": session_id,
                "last_active_ts": ts,
                "status": status,
                "summary": summary,
            }));
        }

        // Sort by last_active_ts descending
        results.sort_by(|a, b| {
            let ta = a["last_active_ts"].as_u64().unwrap_or(0);
            let tb = b["last_active_ts"].as_u64().unwrap_or(0);
            tb.cmp(&ta)
        });

        Ok(serde_json::Value::Array(results))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use crate::session::transcript::{AnyEvent, append_event, TurnUsage, TurnOutput};

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
    async fn list_agents_returns_active_sessions_only() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        crate::git::create_session_worktree(ws.path(), "srunning1").unwrap();
        let tx1 = ws.path().join("sessions/srunning1/transcript.jsonl");
        append_event(&tx1, &AnyEvent::UserTurn {
            content: "task".into(), from: None, context: None, ts: 100,
        }).unwrap();
        crate::git::commit_session(ws.path(), "srunning1", "session(srunning1): init").unwrap();

        crate::git::create_session_worktree(ws.path(), "sdone1").unwrap();
        let tx2 = ws.path().join("sessions/sdone1/transcript.jsonl");
        append_event(&tx2, &AnyEvent::TurnComplete {
            usage: TurnUsage { input: 1, output: 1, cache_read: 0, cache_write: 0 },
            codebase_hash: "abc".into(),
            codebase_dirty: false,
            output: TurnOutput { summary: "done".into(), result: None },
            ts: 200,
        }).unwrap();
        crate::git::commit_session(ws.path(), "sdone1", "session(sdone1): done").unwrap();

        let tool = ListAgentsTool::new(ws.path().to_path_buf());
        let result = tool.execute(serde_json::json!({})).await.unwrap();

        let sessions: Vec<serde_json::Value> = serde_json::from_value(result).unwrap();
        let ids: Vec<&str> = sessions.iter()
            .map(|s| s["session_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"srunning1"), "running session missing from results: {:?}", ids);
        assert!(!ids.contains(&"sdone1"), "done session should be filtered out: {:?}", ids);
    }

    #[tokio::test]
    async fn list_agents_empty_workspace_returns_empty_array() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        let tool = ListAgentsTool::new(ws.path().to_path_buf());
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        let sessions: Vec<serde_json::Value> = serde_json::from_value(result).unwrap();
        assert!(sessions.is_empty());
    }
}
