//! One gait-workflow test program instead of many: one link. Each module is a
//! former standalone `tests/*.rs` target, unmodified.
//!
//! The integration tests that cannot share this process keep their own
//! [[test]] target, listed in Cargo.toml with their reasons.

mod continue_resume;
mod cross_consumer;
mod dts_golden;
mod emit_lock;
mod hook_concurrency;
mod interactive_suspension;
mod lazy_run_infra;
mod no_daemon_threads;
mod observer_parity;
mod output_parity;
mod resume_exit;
mod resume_guard;
mod run_lock;
mod script_basic;
mod script_gate;
mod script_gate_edges;
mod script_replay;
mod script_resume_edges;
mod script_retry;
mod session_log_purity;
mod shell_env;
mod wayfinder_poc;
