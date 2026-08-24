//! Live-tailing a running gait session's log — see
//! docs/superpowers/specs/2026-08-24-gait-watch-crate-design.md.

pub mod error;
pub mod parse;

pub use error::WatchError;
