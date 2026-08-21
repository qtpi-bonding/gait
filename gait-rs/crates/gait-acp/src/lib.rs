//! gait-acp — a JSON-RPC-over-stdio ACP adapter for gait. Phase 1: connection
//! skeleton. This is the library target; the binary (`main.rs`) is a thin shell.
//! Modules (`session_map`, `handlers`) and `build_agent` are added in later tasks
//! and MUST be `pub` so the integration tests under `tests/` can import them.
//! See docs/plans/2026-07-09-acp-adapter-design.md.
pub mod fs_delegate;
pub mod handlers;
pub mod lifecycle;
mod meta_provenance;
pub mod session_map;
pub mod turn_io;

pub use handlers::build_agent;
pub use session_map::SessionMap;

/// Serve the gait-acp agent over real stdio. This is the binary's entry point.
pub async fn run() -> anyhow::Result<()> {
    let map = SessionMap::new();
    let result = build_agent(map.clone())
        .connect_to(agent_client_protocol::Stdio::new())
        .await;
    // Connection closed (editor disconnected): reap every in-flight turn so no
    // thread or sealed brain outlives the connection (design §4a).
    map.drain();
    result.map_err(|e| anyhow::anyhow!("gait-acp connection error: {e}"))
}
use gait_core::embed::FsDelegate as _;

/// Test-only JSON-RPC request/response pair used by
/// `build_agent_with_slow_handler` to prove that a handler which spawns work
/// and returns immediately does not block the read loop. `#[doc(hidden)]
/// pub` (not `#[cfg(test)]`) because the concurrency test lives in
/// `tests/skeleton.rs`, a separate crate compiled without this crate's
/// `cfg(test)`.
#[doc(hidden)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SlowRequest {}

#[doc(hidden)]
impl agent_client_protocol::JsonRpcMessage for SlowRequest {
    fn matches_method(method: &str) -> bool {
        method == "_gait_acp_test/slow"
    }

    fn method(&self) -> &'static str {
        "_gait_acp_test/slow"
    }

    fn to_untyped_message(
        &self,
    ) -> Result<agent_client_protocol::UntypedMessage, agent_client_protocol::Error> {
        agent_client_protocol::UntypedMessage::new(self.method(), self)
    }

    fn parse_message(
        method: &str,
        params: &impl serde::Serialize,
    ) -> Result<Self, agent_client_protocol::Error> {
        if !Self::matches_method(method) {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        agent_client_protocol::util::json_cast_params(params)
    }
}

#[doc(hidden)]
impl agent_client_protocol::JsonRpcRequest for SlowRequest {
    type Response = SlowResponse;
}

#[doc(hidden)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SlowResponse {}

#[doc(hidden)]
impl agent_client_protocol::JsonRpcResponse for SlowResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, agent_client_protocol::Error> {
        serde_json::to_value(self).map_err(agent_client_protocol::Error::into_internal_error)
    }

    fn from_value(
        _method: &str,
        value: serde_json::Value,
    ) -> Result<Self, agent_client_protocol::Error> {
        agent_client_protocol::util::json_cast(&value)
    }
}

/// Same as `build_agent`, plus a `_gait_acp_test/slow` handler that spawns
/// (via `ConnectionTo::spawn`) a task awaiting `release` before responding,
/// returning control to the read loop immediately. Used only by the Task 5
/// concurrency test — see `handlers.rs`'s top-of-file comment for the
/// finding this proves: handlers run inside the dispatch loop and block
/// further message processing until they return, so any handler that must
/// do slow work (Phase 2's `session/prompt`) has to spawn and return early.
#[doc(hidden)]
pub fn build_agent_with_slow_handler(
    map: SessionMap,
    release: tokio::sync::oneshot::Receiver<()>,
) -> impl agent_client_protocol::ConnectTo<agent_client_protocol::Client> {
    use std::sync::Mutex;
    let release = std::sync::Arc::new(Mutex::new(Some(release)));
    handlers::build_agent(map).on_receive_request(
        move |_req: SlowRequest,
              responder: agent_client_protocol::Responder<SlowResponse>,
              cx: agent_client_protocol::ConnectionTo<agent_client_protocol::Client>| {
            let release = release
                .lock()
                .unwrap()
                .take()
                .expect("slow handler invoked more than once in this test harness");
            async move {
                cx.spawn(async move {
                    let _ = release.await;
                    responder.respond(SlowResponse {})
                })?;
                Ok(())
            }
        },
        agent_client_protocol::on_receive_request!(),
    )
}

/// Test-only probe: drives `AcpFsDelegate` from inside a real agent→client
/// connection. `write: None` performs a delegated read; `write: Some(body)`
/// performs a delegated write. Mirrors `SlowRequest` (see above): `#[doc(hidden)]
/// pub` so `tests/fs_delegate.rs` (a separate crate) can construct it.
#[doc(hidden)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FsProbeRequest {
    pub session_id: agent_client_protocol::schema::v1::SessionId,
    pub path: String,
    pub write: Option<String>,
}

#[doc(hidden)]
impl agent_client_protocol::JsonRpcMessage for FsProbeRequest {
    fn matches_method(method: &str) -> bool {
        method == "_gait_acp_test/fs_probe"
    }
    fn method(&self) -> &'static str {
        "_gait_acp_test/fs_probe"
    }
    fn to_untyped_message(
        &self,
    ) -> Result<agent_client_protocol::UntypedMessage, agent_client_protocol::Error> {
        agent_client_protocol::UntypedMessage::new(self.method(), self)
    }
    fn parse_message(
        method: &str,
        params: &impl serde::Serialize,
    ) -> Result<Self, agent_client_protocol::Error> {
        if !Self::matches_method(method) {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        agent_client_protocol::util::json_cast_params(params)
    }
}

#[doc(hidden)]
impl agent_client_protocol::JsonRpcRequest for FsProbeRequest {
    type Response = FsProbeResponse;
}

#[doc(hidden)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FsProbeResponse {
    pub content: Option<String>,
    pub wrote: bool,
    pub error: Option<String>,
}

#[doc(hidden)]
impl agent_client_protocol::JsonRpcResponse for FsProbeResponse {
    fn into_json(self, _method: &str) -> Result<serde_json::Value, agent_client_protocol::Error> {
        serde_json::to_value(self).map_err(agent_client_protocol::Error::into_internal_error)
    }
    fn from_value(
        _method: &str,
        value: serde_json::Value,
    ) -> Result<Self, agent_client_protocol::Error> {
        agent_client_protocol::util::json_cast(&value)
    }
}

/// Same as `build_agent`, plus a `_gait_acp_test/fs_probe` handler that builds an
/// `AcpFsDelegate` from the live `cx` and runs one read or write. It spawns (via
/// `ConnectionTo::spawn`) and responds from the spawned task — a handler that
/// awaited the client inline would deadlock the dispatch loop (see the finding
/// documented atop `handlers.rs`). Used only by `tests/fs_delegate.rs`.
#[doc(hidden)]
pub fn build_agent_with_fs_probe(
    map: SessionMap,
) -> impl agent_client_protocol::ConnectTo<agent_client_protocol::Client> {
    handlers::build_agent(map).on_receive_request(
        move |req: FsProbeRequest,
              responder: agent_client_protocol::Responder<FsProbeResponse>,
              cx: agent_client_protocol::ConnectionTo<agent_client_protocol::Client>| {
            async move {
                let del_cx = cx.clone();
                cx.spawn(async move {
                    let del = crate::fs_delegate::AcpFsDelegate {
                        cx: del_cx,
                        session_id: req.session_id,
                    };
                    let path = std::path::PathBuf::from(req.path);
                    let resp = match req.write {
                        None => match del.read_text_file(&path).await {
                            Ok(content) => FsProbeResponse {
                                content,
                                wrote: false,
                                error: None,
                            },
                            Err(e) => FsProbeResponse {
                                content: None,
                                wrote: false,
                                error: Some(e.to_string()),
                            },
                        },
                        Some(body) => match del.write_text_file(&path, &body).await {
                            Ok(()) => FsProbeResponse {
                                content: None,
                                wrote: true,
                                error: None,
                            },
                            Err(e) => FsProbeResponse {
                                content: None,
                                wrote: false,
                                error: Some(e.to_string()),
                            },
                        },
                    };
                    responder.respond(resp)
                })?;
                Ok(())
            }
        },
        agent_client_protocol::on_receive_request!(),
    )
}
