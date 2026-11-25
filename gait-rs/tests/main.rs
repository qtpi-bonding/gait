//! All gait-core integration tests that share no process-global state live in
//! this one test program: one binary instead of ~25 means one dependency
//! compile and one link. Each module is a former standalone `tests/*.rs`
//! target, unmodified except where a shared `mod common;` was needed.
//!
//! Tests that cannot share this process keep their own [[test]] target in
//! Cargo.toml (sealing tests, telemetry-pipeline tests, and the others whose
//! reasons are listed there).

mod boundary_guard;
mod common;
mod embed_surface;
mod finished_log_path_one_owner;
mod integration_anthropic;
mod integration_commands;
mod integration_coordination;
mod integration_defaults;
mod integration_embed;
mod integration_openai;
mod integration_run_loop;
mod integration_tools;
mod integration_turn;
mod merge_guard;
mod min_no_extensions;
mod provider_key_names_one_table;
mod run_output_split;
mod scope_check_one_door;
mod shell_override_one_reader;
mod spine_checkpoint_enrich;
mod spine_dispatch;
mod spine_locking;
mod spine_persistence;
mod spine_relay_arms;
mod spine_smoke;
mod spine_terminal_failure;
mod test_binaries;
mod unlock_grants_one_reader;
mod workflow_runner_removed;
