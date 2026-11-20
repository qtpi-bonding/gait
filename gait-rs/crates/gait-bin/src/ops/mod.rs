//! gait's operation registry: the user-facing management commands, declared
//! once. The ops arrive domain by domain; this module holds the context, the
//! process globals and the hooks they all share, and the turn, which is the
//! registry's free-text fallback.
mod context;
mod hooks;
pub mod turn;

pub use context::{GaitContext, Globals};
pub use hooks::{GaitCallHook, GaitHooks};
pub use turn::Repl;

/// The `ops!` block. It sits in its own module so `registry()` below can add
/// the call hook to the registry it generates.
pub mod decl {
    use super::{GaitContext, Globals};

    op_registry::ops! {
        Ctx = GaitContext;
        globals = Globals;
        pillar = "gait";
        bin = "gait"; version = env!("CARGO_PKG_VERSION"); about = "AI coding agent";
        groups = {};
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
