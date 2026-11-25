//! One gait-workflow test program instead of many: one link. Each module is a
//! former standalone `tests/*.rs` target, unmodified.
//!
//! The integration tests that cannot share this process keep their own
//! [[test]] target, listed in Cargo.toml with their reasons.

mod branch_condition_errors;
mod continue_resume;
mod cross_consumer;
mod emit_lock;
mod failed_run_pinned;
mod graph_validation;
mod hook_concurrency;
mod interactive_suspension;
mod lazy_run_infra;
mod legacy_log;
mod no_daemon_threads;
mod no_progress_yaml;
mod observer_parity;
mod output_parity;
mod resume_exit;
mod resume_guard;
mod run_lock;
mod script_basic;
mod script_replay;
mod session_log_purity;
mod shell_env;
mod wayfinder_poc;
