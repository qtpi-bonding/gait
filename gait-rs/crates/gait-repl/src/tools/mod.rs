//! REPL-specific agent tools that the model can call. These are injected
//! into the resolved profile by `ReplDriver::resolve_profile` on top of
//! the base `gait-core` tool set, and the host-side dispatcher (also
//! `ReplDriver`) routes the model's tool call into the right `execute`
//! function. Today: `run_workflow` (a thin trampoline onto
//! `gait-workflow::run_workflow`).
