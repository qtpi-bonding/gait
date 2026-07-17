//! gait-state — the persisted workspace state and deterministic operations over it
//! (git, session log, permissions, config resolution, registry, auth, daemons, and
//! the proto event facade). No model turn ever runs here. See
//! docs/plans/2026-07-09-state-engine-split-design.md.

// Base-leaf re-exports so moved modules' `crate::config` / `crate::proto` /
// `crate::process` / `crate::types` / `crate::diagnostic` paths keep resolving
// with no edits (mirrors the re-exports gait-core historically provided).
pub use gait_proto::{envelope, gait, hash, wire, workflow_envelope};
pub use gait_sandbox::{process, sandbox};
pub use gait_types::{config, diagnostic, sandbox_grant, types};
// `auth.rs` / `models_meta.rs` reach `gait_provider::providers::lookup` via the
// extern crate path; re-export the provider surface too so any `crate::provider`
// / `crate::providers` reference inside moved modules keeps resolving.
pub use gait_provider::{anthropic, factory, openai, provider, providers, strict, transport};

pub mod agent_frontmatter;
pub mod auth;
pub mod config_hook;
pub mod daemon_exit;
pub mod gait_dir;
pub mod git;
pub mod hook_runner; // reclassified to state (only config_hook uses it; no turn dep)
pub mod inferences;
pub mod models;
pub mod models_meta;
pub mod permissions;
pub mod proto; // the facade: events, emitter (Recorder), error
pub mod registry;
pub mod session;
pub mod skills;
// hashline / input-subset / harness: add here ONLY if the isolation build proves
// gait-state needs them.
