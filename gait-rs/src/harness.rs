use std::path::PathBuf;
use crate::turn::{TurnResult, SessionLock};
use crate::session::transcript::{append_event, AnyEvent, now_ms};

#[async_trait::async_trait]
pub trait HarnessBackend: Send + Sync {
    async fn after_turn(
        &self,
        session_id: &str,
        assigned_by: Option<&str>,
        result: &TurnResult,
    ) -> anyhow::Result<()>;
}

pub struct NoopHarnessBackend;

#[async_trait::async_trait]
impl HarnessBackend for NoopHarnessBackend {
    async fn after_turn(&self, _: &str, _: Option<&str>, _: &TurnResult) -> anyhow::Result<()> {
        Ok(())
    }
}

pub struct NativeHarnessBackend {
    workspace: PathBuf,
}

impl NativeHarnessBackend {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait::async_trait]
impl HarnessBackend for NativeHarnessBackend {
    async fn after_turn(
        &self,
        session_id: &str,
        assigned_by: Option<&str>,
        result: &TurnResult,
    ) -> anyhow::Result<()> {
        let Some(assigner_id) = assigned_by else { return Ok(()); };

        let message = match result {
            TurnResult::Done { message, .. } => message.chars().take(500).collect::<String>(),
            TurnResult::Permissions => "needs permissions approval".to_string(),
            TurnResult::Clarify { .. } => "paused for clarification".to_string(),
            TurnResult::Inform { .. } => "paused to inform user".to_string(),
        };

        let status = match result {
            TurnResult::Done { .. } => "done",
            TurnResult::Permissions => "permissions",
            TurnResult::Clarify { .. } => "clarify",
            TurnResult::Inform { .. } => "inform",
        };

        let event = AnyEvent::AgentUpdate {
            session_id: session_id.to_string(),
            status: status.to_string(),
            summary: message,
            items: None,
            ts: now_ms(),
        };

        let assigner_dir = self.workspace.join("sessions").join(assigner_id);
        let tx_path = assigner_dir.join("transcript.jsonl");

        // C2 fix: only retry on "already running" — fail fast on I/O errors (dir missing, etc.)
        let mut lock: Option<SessionLock> = None;
        for _ in 0..20 {
            match SessionLock::acquire(&assigner_dir) {
                Ok(l) => { lock = Some(l); break; }
                Err(e) => {
                    if !e.to_string().contains("already running") {
                        return Err(e.context(format!(
                            "unexpected error acquiring lock on assigner session {}", assigner_id
                        )));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
        let _lock = lock.ok_or_else(|| {
            anyhow::anyhow!("failed to acquire lock on assigner session {} after 20 attempts", assigner_id)
        })?;

        append_event(&tx_path, &event)?;
        let repos = crate::git::Repos::production();
        repos.traces.commit_session(
            &self.workspace,
            assigner_id,
            &format!("session({}): agent_update from {}", assigner_id, session_id),
        )?;
        drop(_lock);

        // Spawn gait run for the assigner (detached — don't wait)
        let exe = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("gait"));
        std::process::Command::new(&exe)
            .args([
                "run",
                "--session", assigner_id,
                "--workspace", self.workspace.to_str().unwrap_or("."),
                "--auto",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::TurnResult;

    #[tokio::test]
    async fn noop_harness_always_succeeds() {
        let h = NoopHarnessBackend;
        h.after_turn("s1", None, &TurnResult::Done { message: "done".into(), result: None }).await.unwrap();
        h.after_turn("s1", Some("s-parent"), &TurnResult::Permissions).await.unwrap();
    }

    #[tokio::test]
    async fn native_harness_with_no_assigned_by_is_noop() {
        let h = NativeHarnessBackend::new(std::path::PathBuf::from("/tmp/fake-ws"));
        // assigned_by = None → no action, returns Ok
        h.after_turn("s1", None, &TurnResult::Done { message: "done".into(), result: None }).await.unwrap();
    }
}
