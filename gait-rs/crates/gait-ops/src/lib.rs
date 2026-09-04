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

/// Shared JSON renderer for every `Outcome::to_json(pretty)` in this crate —
/// the CLI's only output format (design decision 2026-08-30: no plain-text
/// rendering in gait-cli, `pretty` replaces the old `--json`/plain split).
pub fn render_json<T: serde::Serialize>(value: &T, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(value).unwrap_or_default()
    } else {
        serde_json::to_string(value).unwrap_or_default()
    }
}

// Domain modules — filled in by later tasks. Declared now so the tree exists.
pub mod attestation;
pub mod auth;
pub mod config;
pub mod daemon;
pub mod extension;
pub mod identity;
pub mod inference;
pub mod interaction;
pub mod permission;

pub mod session;
pub mod workspace;
