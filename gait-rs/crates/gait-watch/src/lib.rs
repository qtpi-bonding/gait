//! Live-tailing a running gait session's log — see
//! docs/superpowers/specs/2026-08-24-gait-watch-crate-design.md.

pub mod error;
pub mod hub;
pub mod parse;
pub mod tail;

pub use error::WatchError;
pub use hub::{HubEvent, SessionWatchHub, SubscriptionHandle};
pub use tail::watch_session_log;
