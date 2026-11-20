//! gait's operation registry: the user-facing management commands, declared
//! once. The ops arrive domain by domain; this module holds the context, the
//! process globals and the hooks they all share.
mod context;
mod hooks;

pub use context::{GaitContext, Globals};
pub use hooks::{GaitCallHook, GaitHooks};

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

/// The registry of gait's operations, with the telemetry call hook set.
pub fn registry() -> op_registry::Registry<GaitContext> {
    let mut registry = decl::registry();
    registry.set_call_hook(std::sync::Arc::new(GaitCallHook));
    registry
}
