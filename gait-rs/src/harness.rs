use crate::scheduler::Action;
use std::path::PathBuf;
use tokio::sync::mpsc;

#[async_trait::async_trait]
pub trait HarnessBackend: Send + Sync {
    /// Execute a scheduler-produced action (mechanism only).
    async fn execute(&self, action: &Action) -> anyhow::Result<()>;
}

pub struct NoopHarnessBackend;

#[async_trait::async_trait]
impl HarnessBackend for NoopHarnessBackend {
    async fn execute(&self, _action: &Action) -> anyhow::Result<()> {
        Ok(())
    }
}

pub struct NotifyPayload {
    pub target_session: String,
    pub from_session: String,
    pub status: String,
    pub summary: String,
}

pub struct NativeHarnessBackend {
    gait_dir: PathBuf,
    /// When `Some`, route `gait run` spawns via the Nerve's Spawn signal instead
    /// of calling `Command::spawn` directly (required when brain is sealed).
    /// NOTE: spawn_tx is provisioned by the brain for future use; the Notify path
    /// now routes through notify_parent (spine calls spawn_wake directly).
    #[allow(dead_code)]
    spawn_tx: Option<mpsc::UnboundedSender<(Vec<String>, bool)>>,
    /// When `Some`, route NotifyAndResume writes via the Nerve's Notify signal
    /// instead of writing directly to disk (required when brain is sealed).
    notify_tx: Option<tokio::sync::mpsc::UnboundedSender<NotifyPayload>>,
}

impl NativeHarnessBackend {
    pub fn new(gait_dir: PathBuf) -> Self {
        Self {
            gait_dir,
            spawn_tx: None,
            notify_tx: None,
        }
    }

    pub fn new_with_spawn(
        gait_dir: PathBuf,
        spawn_tx: mpsc::UnboundedSender<(Vec<String>, bool)>,
    ) -> Self {
        Self {
            gait_dir,
            spawn_tx: Some(spawn_tx),
            notify_tx: None,
        }
    }

    pub fn with_notify_tx(mut self, tx: tokio::sync::mpsc::UnboundedSender<NotifyPayload>) -> Self {
        self.notify_tx = Some(tx);
        self
    }
}

#[async_trait::async_trait]
impl HarnessBackend for NativeHarnessBackend {
    async fn execute(&self, action: &Action) -> anyhow::Result<()> {
        match action {
            Action::NotifyAndResume {
                target_session,
                from_session,
                status,
                summary,
            } => {
                if let Some(ref tx) = self.notify_tx {
                    // Sealed brain path: delegate write + commit + wake to spine via Nerve.
                    // The spine's Notify handler calls notify_parent (one-shot lock acquire,
                    // write, commit, spawn_wake) — no duplicate spawn needed here.
                    let payload = NotifyPayload {
                        target_session: target_session.clone(),
                        from_session: from_session.clone(),
                        status: status.clone(),
                        summary: summary.clone(),
                    };
                    let _ = tx.send(payload);
                } else {
                    // Non-sealed path: use notify_parent — one-shot lock acquire, dedup,
                    // write, commit, and wake. Replaces the 20×500ms retry loop.
                    // ParentBusy → the live turn's per-step poll will surface it.
                    let c = crate::session::completion::Completion::Agent {
                        from: from_session.clone(),
                        status: status.clone(),
                        summary: summary.clone(),
                    };
                    let repos = crate::git::Repos::production();
                    match crate::session::completion::notify_parent(
                        &self.gait_dir,
                        &repos,
                        target_session,
                        &c,
                    ) {
                        crate::session::completion::NotifyOutcome::Woke(req) => {
                            crate::launch::spawn_wake(&req);
                        }
                        crate::session::completion::NotifyOutcome::ParentBusy => {}
                        crate::session::completion::NotifyOutcome::AlreadySurfaced => {}
                        crate::session::completion::NotifyOutcome::NoParent => {
                            eprintln!(
                                "[harness] notify: parent session {} not found",
                                target_session
                            );
                        }
                    }
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_harness_execute_succeeds() {
        let action = Action::NotifyAndResume {
            target_session: "parent".into(),
            from_session: "child".into(),
            status: "done".into(),
            summary: "x".into(),
        };
        assert!(NoopHarnessBackend.execute(&action).await.is_ok());
    }
}
