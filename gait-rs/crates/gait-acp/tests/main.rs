//! One gait-acp test program instead of many: one link. Each module is a
//! former standalone `tests/*.rs` target, unmodified.
//!
//! The integration tests that cannot share this process keep their own
//! [[test]] target, listed in Cargo.toml with their reasons.

mod lifecycle;
mod load;
mod merge_guard;
mod mode;
mod role_dispatch_identity;
mod run_tool_hand;
mod skeleton;
mod startup_fail_closed;
mod startup_identity_fail_closed;
