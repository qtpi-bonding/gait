//! One gait-cli test program instead of many: one link. Each module is a
//! former standalone `tests/*.rs` target, unmodified.
//!
//! The integration tests that cannot share this process keep their own
//! [[test]] target, listed in Cargo.toml with their reasons.

mod reply;
mod workflow_cli;
