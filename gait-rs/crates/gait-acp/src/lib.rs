//! gait-acp — a JSON-RPC-over-stdio ACP adapter for gait. Phase 1: connection
//! skeleton. This is the library target; the binary (`main.rs`) is a thin shell.
//! Modules (`session_map`, `handlers`) and `build_agent` are added in later tasks
//! and MUST be `pub` so the integration tests under `tests/` can import them.
//! See docs/plans/2026-07-09-acp-adapter-design.md.
pub mod handlers;
pub mod session_map;

pub use handlers::build_agent;
pub use session_map::SessionMap;

/// Serve the gait-acp agent over real stdio. This is the binary's entry point.
pub async fn run() -> anyhow::Result<()> {
    use agent_client_protocol::ConnectTo;
    build_agent(SessionMap::new())
        .connect_to(agent_client_protocol::Stdio::new())
        .await
        .map_err(|e| anyhow::anyhow!("gait-acp connection error: {e}"))
}
