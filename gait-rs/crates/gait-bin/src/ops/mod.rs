//! gait's operation registry: the user-facing management commands, declared
//! once. The ops arrive domain by domain; this module holds the context, the
//! process globals and the hooks they all share, and the turn, which is the
//! registry's free-text fallback.
/// Declares an op output struct: serialized in camelCase, with a JSON schema
/// under the `schema` feature. The doc comments are the schema descriptions.
macro_rules! output {
    ($($(#[$meta:meta])* pub struct $name:ident { $($body:tt)* })*) => {$(
        $(#[$meta])*
        #[derive(Debug, Clone, serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        #[cfg_attr(feature = "schema", derive(schemars_v1::JsonSchema))]
        #[cfg_attr(feature = "schema", schemars(crate = "schemars_v1"))]
        pub struct $name { $($body)* }
    )*};
}

mod context;
mod hooks;
pub mod session;
pub mod turn;

pub use context::{GaitContext, Globals};
pub use hooks::{GaitCallHook, GaitHooks};
pub use turn::Repl;

/// The `ops!` block. It sits in its own module so `registry()` below can add
/// the call hook to the registry it generates.
pub mod decl {
    use super::session::{
        AllowParams, BulkParams, PsParams, ReplyParams, SessionId, SessionsParams, UndoParams,
        UnlockParams,
    };
    use super::session::{
        AllowResult, DenyResult, ProcessItem, ReplyResult, SessionAct, SessionItem, SessionStats,
        SessionSummary, UndoResult, UnlockResult,
    };
    use super::{GaitContext, Globals};
    use gait_ops::OpError;

    op_registry::ops! {
        Ctx = GaitContext;
        globals = Globals;
        pillar = "gait";
        bin = "gait"; version = env!("CARGO_PKG_VERSION"); about = "AI coding agent";
        groups = {};
        /// List the active sessions, most recently updated first.
        "sessions" => { params: SessionsParams, output: Vec<SessionItem>,
                        access: reads, audience: owner, op: super::session::sessions,
                        errors: [OpError] },
        /// Show each mounted session and its live state.
        "ps" => { params: PsParams, output: Vec<ProcessItem>,
                  access: reads, audience: owner, op: super::session::ps,
                  errors: [OpError] },
        /// Show a session's token use, split into agent work and compaction.
        "stats" => { params: SessionId, output: SessionStats,
                     access: reads, audience: owner, op: super::session::stats,
                     errors: [OpError] },
        /// Show the outcome of a session's last completed run: its message, checkpoint, and the files and tools it touched.
        "log" => { params: SessionId, output: SessionSummary,
                   access: reads, audience: owner, op: super::session::log,
                   errors: [OpError] },
        /// Go back to an earlier turn of a session, or list the turns with `--list`.
        "undo" => { params: UndoParams, output: UndoResult,
                    access: [writes, destructive], audience: owner, op: super::session::undo,
                    errors: [OpError] },
        /// Merge a session onto main and remove its worktree, or all idle sessions with `--all --yes`.
        "persist" => { params: BulkParams, output: SessionAct,
                       access: writes, audience: owner, op: super::session::persist,
                       errors: [OpError] },
        /// Remove a session's worktree without merging; unrecoverable. `--all --yes --force` does it for all idle sessions.
        "discard" => { params: BulkParams, output: SessionAct,
                       access: [writes, destructive], audience: owner, op: super::session::discard,
                       errors: [OpError] },
        /// Approve the permission requests a session is waiting on.
        "allow" => { params: AllowParams, output: AllowResult,
                     access: writes, audience: owner, op: super::session::allow,
                     errors: [OpError] },
        /// Refuse the permission requests a session is waiting on.
        "deny" => { params: SessionId, output: DenyResult,
                    access: writes, audience: owner, op: super::session::deny,
                    errors: [OpError] },
        /// Approve the sandbox unlock requests a session is waiting on.
        "unlock" => { params: UnlockParams, output: UnlockResult,
                      access: writes, audience: owner, op: super::session::unlock,
                      errors: [OpError] },
        /// Answer the single clarify or inform question a session is waiting on.
        "reply" => { params: ReplyParams, output: ReplyResult,
                     access: writes, audience: owner, op: super::session::reply,
                     errors: [OpError] },
    }
}

/// The registry of the full `gait`: its operations, the telemetry call hook and
/// the turn fallback.
#[cfg(feature = "rich-tui")]
pub fn registry() -> op_registry::Registry<GaitContext> {
    registry_with(Repl::Rich)
}

/// The registry of gait without the themed REPL.
#[cfg(not(feature = "rich-tui"))]
pub fn registry() -> op_registry::Registry<GaitContext> {
    registry_with(Repl::Plain)
}

/// The registry of `gait-min`: the same operations and turn, the plain REPL, and
/// its own name.
pub fn min_registry() -> op_registry::Registry<GaitContext> {
    let mut registry = registry_with(Repl::Plain);
    registry.set_bin(
        "gait-min",
        env!("CARGO_PKG_VERSION"),
        "AI coding agent (minimal build)",
    );
    registry
}

/// A registry whose turn opens `repl` for `-i`.
pub fn registry_with(repl: Repl) -> op_registry::Registry<GaitContext> {
    let mut registry = decl::registry();
    registry.set_call_hook(std::sync::Arc::new(GaitCallHook));
    match repl {
        #[cfg(feature = "rich-tui")]
        Repl::Rich => registry.fallback_free_text("turn", turn::TURN_HELP, turn::TURN_FLAGS, turn::run_rich),
        Repl::Plain => registry.fallback_free_text("turn", turn::TURN_HELP, turn::TURN_FLAGS, turn::run_plain),
    }
    registry
}

/// Writes `error` as the JSON error line on stderr, records it, and returns its
/// exit code (2 for the usage codes). For a failure the registry's runner does
/// not see, such as the turn's own flags.
pub fn usage_error_exit(error: anyhow::Error) -> u8 {
    use std::io::IsTerminal;
    #[cfg(feature = "telemetry")]
    concord_telemetry::error_anyhow!(&error, source = "gait.cli");
    let mut stderr = std::io::stderr();
    let tty = stderr.is_terminal();
    let _ = op_registry::write_error(&mut stderr, &error, &op_registry::Redactor::default(), tty);
    op_registry::exit::exit_code(&error)
}
