//! gait-ops — the deterministic command verbs (link, init, session admin,
//! permissions, extensions, auth, daemons), shared by the CLI, REPL, and ACP
//! adapter. NO verb here ever runs a model turn (design §2). See
//! docs/plans/2026-07-09-core-ops-layer-design.md.
//!
//! Wall 1: this crate never touches the terminal or exits — output is returned in
//! an `Outcome`, failure in `OpError`. Enforced crate-wide, no exceptions.
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

mod error;
pub use error::OpError;

// Domain modules — filled in by later tasks. Declared now so the tree exists.
pub mod auth;
pub mod config;
pub mod daemon;
pub mod extension;
pub mod inference;
pub mod interaction;
pub mod permission;

pub mod session;
pub mod workspace;
