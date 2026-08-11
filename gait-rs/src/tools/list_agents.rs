use crate::proto::gait::log::v1::{event, payload, Event};
use crate::tools::ToolCtx;
use crate::types::{ExecutionMode, Tool};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct ListAgentsParams {}

#[allow(dead_code)]
#[derive(schemars::JsonSchema)]
struct AgentEntry {
    id: String,
    status: String,
    task: String,
}

#[allow(dead_code)]
#[derive(schemars::JsonSchema)]
struct ListAgentsOutput {
    agents: Vec<AgentEntry>,
}

pub struct ListAgentsTool {
    ctx: ToolCtx,
    parameters: Value,
    output_schema: Value,
}

impl ListAgentsTool {
    pub fn new(ctx: ToolCtx) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<ListAgentsParams>(),
            crate::types::OutputFilter::Both,
        );
        Self {
            ctx,
            parameters,
            output_schema: crate::tools::make_schema::<ListAgentsOutput>(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for ListAgentsTool {
    fn name(&self) -> &str {
        "list_agents"
    }
    fn description(&self) -> &str {
        "List active agent sessions not yet done, sorted by last activity descending."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&self.output_schema)
    }

    async fn execute(
        &self,
        _args: Value,
        _rec: &dyn crate::types::Rec,
    ) -> Result<Value, crate::tools::ToolError> {
        // C1 fix: read transcript from the branch's committed git tree, NOT from the filesystem
        // working copy. Worktree dirs may be cleaned up after finish_session; the branch remains.
        let repo = match git2::Repository::open(&self.ctx.gait_dir) {
            Ok(r) => r,
            Err(_) => return Ok(serde_json::json!([])),
        };

        let mut results: Vec<serde_json::Value> = vec![];

        let branches = match repo.branches(Some(git2::BranchType::Local)) {
            Ok(b) => b,
            Err(_) => return Ok(serde_json::json!([])),
        };

        for branch_result in branches {
            let (branch, _) = match branch_result {
                Ok(b) => b,
                Err(_) => continue,
            };
            let name = match branch.name() {
                Ok(Some(n)) => n.to_string(),
                _ => continue,
            };
            let session_id = match name.strip_prefix("session/") {
                Some(id) => id.to_string(),
                None => continue,
            };

            // Read log.jsonl blob from the branch's committed tree
            let reference = branch.into_reference();
            let commit = match reference.peel_to_commit() {
                Ok(c) => c,
                Err(_) => continue,
            };
            let tree = match commit.tree() {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Read sessions/<id>/log.jsonl from the tree (the persisted/
            // nested shape every session branch has had since its first,
            // orphan commit — see GitTraceRepo::create_session_worktree),
            // NOT a flat log.jsonl at the tree root.
            let log_entry_path = std::path::Path::new("sessions")
                .join(&session_id)
                .join("log.jsonl");
            let blob_content = match tree
                .get_path(&log_entry_path)
                .ok()
                .and_then(|entry| repo.find_blob(entry.id()).ok())
            {
                Some(blob) => match std::str::from_utf8(blob.content()) {
                    Ok(s) => s.to_string(),
                    Err(_) => continue,
                },
                None => continue,
            };
            let events: Vec<Event> = blob_content
                .lines()
                .filter(|l| !l.is_empty())
                .filter_map(|l| {
                    serde_json::from_str::<Event>(l)
                        .map_err(|e| {
                            eprintln!("[gait] warning: skipping malformed event in agent log: {e}")
                        })
                        .ok()
                })
                .collect();
            if events.is_empty() {
                continue;
            }

            // Guarded by `if events.is_empty() { continue; }` two lines above.
            let last_evt = events.last().expect("guarded by is_empty check above");
            // Env-stamp timestamp lives on the envelope now (not per-variant).
            let ts = last_evt.ts;

            let last_kind = |e: &Event| -> Option<payload::Kind> {
                match &e.body {
                    Some(event::Body::Payload(p)) => p.kind.clone(),
                    _ => None,
                }
            };
            let status = match last_kind(last_evt) {
                Some(payload::Kind::RunComplete(_)) => "done",
                Some(payload::Kind::RunCancelled(_)) => "cancelled",
                Some(payload::Kind::RunFailed(_)) => "error",
                Some(payload::Kind::Error(_)) => "error",
                Some(payload::Kind::Clarify(_)) => "clarify",
                Some(payload::Kind::Inform(_)) => "inform",
                Some(payload::Kind::Permissions(_)) => "permissions",
                _ => "running",
            };

            if status == "done" {
                continue;
            }

            let summary = events
                .iter()
                .rev()
                .find_map(|e| match &e.body {
                    Some(event::Body::Payload(p)) => match p.kind.as_ref() {
                        Some(payload::Kind::AssistantText(a)) => {
                            Some(a.content.chars().take(100).collect::<String>())
                        }
                        _ => None,
                    },
                    _ => None,
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
    use crate::git::trace::{GitTraceRepo, TraceRepo};
    use crate::proto::{events, wire};
    use crate::test_utils::init_git_repo;
    use tempfile::TempDir;

    #[tokio::test]
    async fn list_agents_returns_active_sessions_only() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        GitTraceRepo
            .create_session_worktree(ws.path(), "srunning1")
            .unwrap();
        let tx1 = crate::session::live::log_path(ws.path(), "srunning1");
        std::fs::create_dir_all(tx1.parent().unwrap()).unwrap();
        wire::append(&tx1, events::user_text("task".into())).unwrap();
        GitTraceRepo
            .commit_session(ws.path(), "srunning1", "session(srunning1): init")
            .unwrap();

        GitTraceRepo
            .create_session_worktree(ws.path(), "sdone1")
            .unwrap();
        let tx2 = crate::session::live::log_path(ws.path(), "sdone1");
        std::fs::create_dir_all(tx2.parent().unwrap()).unwrap();
        wire::append(&tx2, events::run_complete(Default::default())).unwrap();
        GitTraceRepo
            .commit_session(ws.path(), "sdone1", "session(sdone1): done")
            .unwrap();

        let tool = ListAgentsTool::new(ToolCtx::new(
            ws.path().to_path_buf(),
            ws.path().to_path_buf(),
            "test".into(),
        ));
        let result = tool
            .execute(serde_json::json!({}), &crate::Recorder::detached())
            .await
            .unwrap();

        let sessions: Vec<serde_json::Value> = serde_json::from_value(result).unwrap();
        let ids: Vec<&str> = sessions
            .iter()
            .map(|s| s["session_id"].as_str().unwrap())
            .collect();
        assert!(
            ids.contains(&"srunning1"),
            "running session missing from results: {:?}",
            ids
        );
        assert!(
            !ids.contains(&"sdone1"),
            "done session should be filtered out: {:?}",
            ids
        );
    }

    #[tokio::test]
    async fn list_agents_reads_the_nested_persisted_log_path() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        GitTraceRepo
            .create_session_worktree(ws.path(), "snested1")
            .unwrap();
        let tx = crate::session::live::log_path(ws.path(), "snested1");
        std::fs::create_dir_all(tx.parent().unwrap()).unwrap();
        wire::append(&tx, events::user_text("task".into())).unwrap();
        GitTraceRepo
            .commit_session(ws.path(), "snested1", "session(snested1): init")
            .unwrap();

        let tool = ListAgentsTool::new(ToolCtx::new(
            ws.path().to_path_buf(),
            ws.path().to_path_buf(),
            "test".into(),
        ));
        let result = tool
            .execute(serde_json::json!({}), &crate::Recorder::detached())
            .await
            .unwrap();
        let sessions: Vec<serde_json::Value> = serde_json::from_value(result).unwrap();
        let ids: Vec<&str> = sessions
            .iter()
            .map(|s| s["session_id"].as_str().unwrap())
            .collect();
        assert!(
            ids.contains(&"snested1"),
            "list_agents must find sessions via the nested sessions/<id>/log.jsonl tree path: {:?}",
            ids
        );
    }

    #[tokio::test]
    async fn list_agents_empty_workspace_returns_empty_array() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        let tool = ListAgentsTool::new(ToolCtx::new(
            ws.path().to_path_buf(),
            ws.path().to_path_buf(),
            "test".into(),
        ));
        let result = tool
            .execute(serde_json::json!({}), &crate::Recorder::detached())
            .await
            .unwrap();
        let sessions: Vec<serde_json::Value> = serde_json::from_value(result).unwrap();
        assert!(sessions.is_empty());
    }
}
