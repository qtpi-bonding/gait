use crate::scheduler::Action;
use std::path::PathBuf;

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
    pub idempotency_key: String,
}

pub struct NativeHarnessBackend {
    gait_dir: PathBuf,
    /// When `Some`, route NotifyAndResume writes via the Nerve's Notify signal
    /// instead of writing directly to disk (required when brain is sealed).
    notify_tx: Option<tokio::sync::mpsc::UnboundedSender<NotifyPayload>>,
    /// Agent identity that signs the parent notification. `None` makes
    /// `notify_parent` return `NoParent`.
    identity: Option<std::sync::Arc<gait_identity::IdentityContext>>,
}

impl NativeHarnessBackend {
    pub fn new(gait_dir: PathBuf) -> Self {
        Self {
            gait_dir,
            notify_tx: None,
            identity: None,
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
                idempotency_key,
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
                        idempotency_key: idempotency_key.clone(),
                    };
                    let _ = tx.send(payload);
                } else {
                    // Non-sealed path: use notify_parent — one-shot lock acquire, dedup,
                    // write, commit, and wake. Replaces the 20×500ms retry loop.
                    // ParentBusy → the live turn's per-step poll will surface it.
                    let c = crate::session::completion::Completion {
                        payload: crate::proto::events::agent_update(
                            from_session.clone(),
                            status,
                            summary.clone(),
                            None,
                            idempotency_key.clone(),
                        ),
                        idempotency_key: idempotency_key.clone(),
                    };
                    let repos = crate::git::Repos::production();
                    let notify_result = crate::session::completion::notify_parent(
                        &self.gait_dir,
                        &repos,
                        target_session,
                        &c,
                        self.identity.as_deref(),
                        &crate::tools::spawn_agent::wall_clock_secs,
                    );
                    #[cfg(feature = "telemetry")]
                    crate::turn::spine::emit_agent_reported(
                        &idempotency_key,
                        target_session,
                        &from_session,
                        &status,
                        &notify_result,
                    );
                    match notify_result {
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
            idempotency_key: "child-evt1".into(),
        };
        assert!(NoopHarnessBackend.execute(&action).await.is_ok());
    }
}
