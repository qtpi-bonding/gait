//! FS delegation for the ACP adapter (design: docs/plans/2026-07-11-acp-adapter-phase5-design.md).
//!
//! Two pieces: `ClientFsCaps` — a connection-level record of which fs methods the
//! client advertised at `initialize`; and `AcpFsDelegate` (Task 2) — the
//! `gait_core::embed::FsDelegate` impl that proxies buffer reads/writes to the
//! client. The adapter does NO scope checking: the spine authorizes every path
//! before the delegate is ever called (src/turn/spine/handlers/tool.rs:74-97).
use std::sync::atomic::{AtomicBool, Ordering};

/// Which filesystem methods the connected client advertised at `initialize`.
/// Connection-scoped: written once by the `initialize` handler, read by each
/// `session/prompt` before it spawns its turn. Ordered write-then-read on the
/// same dispatch task, so `Release`/`Acquire` is belt-and-suspenders.
#[derive(Default)]
pub struct ClientFsCaps {
    read: AtomicBool,
    write: AtomicBool,
}

impl ClientFsCaps {
    /// Record the client's advertised fs capabilities. Last write wins.
    pub fn set(&self, read: bool, write: bool) {
        self.read.store(read, Ordering::Release);
        self.write.store(write, Ordering::Release);
    }

    /// Delegation is installable iff the client supports BOTH read and write.
    /// All-or-nothing: a client that supports only one gets disk fallback, never
    /// a mixed buffer/disk delegate (design §1, Global Constraints).
    pub fn delegable(&self) -> bool {
        self.read.load(Ordering::Acquire) && self.write.load(Ordering::Acquire)
    }
}

use agent_client_protocol::{
    Client, ConnectionTo,
    schema::v1::{ReadTextFileRequest, SessionId, WriteTextFileRequest},
};
use std::path::Path;

/// A `FsDelegate` that proxies buffer reads/writes to the connected ACP client.
/// Holds a clone of the connection handle (`Send + Sync + Clone`) and the
/// session id; every call is one agent→client JSON-RPC round-trip — the same
/// shape the permission loop uses (turn_io.rs). Does NO scope checking (the
/// spine already authorized the path).
pub struct AcpFsDelegate {
    pub cx: ConnectionTo<Client>,
    pub session_id: SessionId,
}

#[async_trait::async_trait]
impl gait_core::embed::FsDelegate for AcpFsDelegate {
    async fn read_text_file(&self, path: &Path) -> anyhow::Result<Option<String>> {
        let req = ReadTextFileRequest::new(self.session_id.clone(), path.to_path_buf());
        match self.cx.send_request(req).block_task().await {
            Ok(resp) => Ok(Some(resp.content)),
            // Error -> empty/new-file (proceed). The editor errors when reading a
            // not-yet-created file; treating that as "no buffer" is what lets gait
            // create new files. A genuine transport failure still surfaces
            // downstream (failed edit-anchor match, or the write-back below).
            Err(_) => Ok(None),
        }
    }

    async fn write_text_file(&self, path: &Path, content: &str) -> anyhow::Result<()> {
        let req = WriteTextFileRequest::new(
            self.session_id.clone(),
            path.to_path_buf(),
            content.to_string(),
        );
        self.cx
            .send_request(req)
            .block_task()
            .await
            .map(|_resp| ())
            // Write error -> hard Err. A failed persist must not report success;
            // the spine withholds the tool's provisional success on Err.
            .map_err(|e| anyhow::anyhow!("fs/write_text_file failed: {e}"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegable_requires_both_flags() {
        let c = ClientFsCaps::default();
        assert!(!c.delegable(), "default: no capabilities");
        c.set(true, false);
        assert!(!c.delegable(), "read-only is not delegable");
        c.set(false, true);
        assert!(!c.delegable(), "write-only is not delegable");
        c.set(true, true);
        assert!(c.delegable(), "both -> delegable");
    }

    #[test]
    fn set_last_write_wins() {
        let c = ClientFsCaps::default();
        c.set(true, true);
        c.set(false, false);
        assert!(!c.delegable(), "a later set overrides an earlier one");
    }
}
