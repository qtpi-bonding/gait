//! MCP (Model Context Protocol) module.
//!
//! - [`client`]: runtime client for talking to MCP servers (connect, list tools, call tools).
//! - [`scanner`]: startup scanner that reads `<gait_dir>/mcp/<name>.json` files into
//!   [`crate::config::McpServerDef`] records.

// `client`'s own `#![cfg(feature = "mcp")]` (first line of client.rs) cfg's
// out the whole module, declaration included — a bare `pub use client::*;`
// with no matching `#[cfg]` here fails to resolve when `mcp` is off, so the
// declaration/glob-use pair below must carry the same gate explicitly.
#[cfg(feature = "mcp")]
pub mod client;
pub mod scanner;

#[cfg(feature = "mcp")]
pub use client::*;
pub use scanner::*;
