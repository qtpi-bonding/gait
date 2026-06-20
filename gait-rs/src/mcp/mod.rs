//! MCP (Model Context Protocol) module.
//!
//! - [`client`]: runtime client for talking to MCP servers (connect, list tools, call tools).
//! - [`scanner`]: startup scanner that reads `<gait_dir>/mcp/<name>.json` files into
//!   [`crate::config::McpServerDef`] records.

pub mod client;
pub mod scanner;

pub use client::*;
pub use scanner::*;
