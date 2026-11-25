//! One gait-bin test program instead of many: one dependency compile and one
//! link. Each module is a former standalone `tests/*.rs` target, unmodified.
//!
//! The integration tests that cannot share this process keep their own
//! [[test]] target, listed in Cargo.toml with their reasons.

mod admin_ops;
mod auth_models;
mod config;
mod exit_codes;
mod gait_min_doc_smoke;
mod merge_guard;
mod min_composition;
mod no_daemon_wire;
mod reply;
mod retired_event_kind;
mod run_tool_hand;
mod sessions_and_log;
mod turn_fallback;
mod workflow_gait_dir_order;
mod workspace;
