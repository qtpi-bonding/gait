//! gait-acp — a JSON-RPC-over-stdio ACP adapter for gait. Phase 1: connection
//! skeleton. This is the library target; the binary (`main.rs`) is a thin shell.
//! Modules (`session_map`, `handlers`) and `build_agent` are added in later tasks
//! and MUST be `pub` so the integration tests under `tests/` can import them.
//! See docs/plans/2026-07-09-acp-adapter-design.md.
pub mod session_map;
