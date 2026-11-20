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

pub mod auth;
pub mod config;
mod context;
pub mod daemon;
pub mod extension;
mod hooks;
pub mod inference;
#[cfg(feature = "local")]
pub mod local;
pub mod mode;
pub mod session;
pub mod turn;

pub use context::{GaitContext, Globals};
pub use hooks::{GaitCallHook, GaitHooks};
pub use turn::Repl;

/// An admin failure that is not the caller's mistake: keeps a code it already
/// has (not found, usage), else is infrastructure (exit 69).
pub(crate) fn infra(error: impl Into<anyhow::Error>) -> anyhow::Error {
    let error = error.into();
    if coded_error::from_anyhow(&error).is_some() {
        error
    } else {
        gait_cli::cli::error::CliError::failed(format!("{error:#}"))
    }
}

/// Fails with `gait.op.not_found` unless `session` has a mounted worktree.
pub(crate) fn require_active(gait_dir: &std::path::Path, session: &str) -> anyhow::Result<()> {
    if gait_core::session::live::worktree_dir(gait_dir, session).exists() {
        Ok(())
    } else {
        Err(coded_error::into_anyhow(gait_ops::OpError::NotFound(format!(
            "session '{session}' is not active"
        ))))
    }
}

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
    use super::auth::{
        AuthEntry, AuthLinkParams, AuthListParams, AuthLogin, AuthLoginParams, AuthLogout,
        AuthLogoutParams, AuthRemoved, AuthSaved, AuthStatus, ModelItem, ModelsParams, ProviderId,
    };
    use super::config::{ConfigEntry, ConfigKey, ConfigSetParams, ConfigShowParams};
    use super::daemon::{DaemonItem, DaemonListParams, DaemonStopParams, DaemonStopped};
    use super::extension::{
        ExtensionLoadParams, ExtensionLoaded, ExtensionRegisterParams, ExtensionRegistered,
        ExtensionToggleParams, ExtensionToggled, ExtensionUnregisterParams, ExtensionUnregistered,
    };
    use super::inference::{
        InferenceAddParams, InferenceAdded, InferenceClearParams, InferenceItem,
        InferenceListParams, InferenceRef, InferenceSelectParams, InferenceSelected,
    };
    use super::mode::{
        ModeAddParams, ModeAdded, ModeClearParams, ModeItem, ModeListParams, ModeRef,
        ModeSelectParams, ModeSelected,
    };
    use super::{GaitContext, Globals};
    use gait_cli::cli::error::CliError;
    use gait_ops::OpError;

    op_registry::ops! {
        Ctx = GaitContext;
        globals = Globals;
        pillar = "gait";
        bin = "gait"; version = env!("CARGO_PKG_VERSION"); about = "AI coding agent";
        groups = {
            "auth" => "Provider credentials: API keys, subscription logins and links to other tools' credentials.",
            "inference" => "Named model profiles, and which one a session uses.",
            "mode" => "Named permission modes, and which one a session uses.",
            "config" => "Keys of the gait directory's gait.jsonc.",
            "daemons" => "Background processes that sessions started.",
        };
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
        /// List the configured providers and what kind of credential each has; never any key text.
        "auth list" => { params: AuthListParams, output: Vec<AuthEntry>,
                         access: reads, audience: owner, op: super::auth::list,
                         errors: [OpError, CliError], default: true },
        /// Show whether a provider has a credential and of what kind, without revealing it.
        "auth status" => { params: ProviderId, output: AuthStatus,
                           access: reads, audience: owner, op: super::auth::status,
                           errors: [OpError, CliError] },
        /// Store an API key for a provider. The key is read from the terminal prompt, or one line of stdin; it is never an argument.
        "auth add" => { params: ProviderId, output: AuthSaved,
                        access: writes, audience: owner, op: super::auth::add,
                        errors: [OpError, CliError] },
        /// Use another tool's credentials for a provider without copying them.
        "auth link" => { params: AuthLinkParams, output: AuthSaved,
                         access: writes, audience: owner, op: super::auth::link,
                         errors: [OpError, CliError] },
        /// Sign in to a subscription provider: a device code for `openai-codex`, a browser step for `openai-signin`.
        "auth login" => { params: AuthLoginParams, output: AuthLogin,
                          access: [writes, network], audience: owner, op: super::auth::login,
                          errors: [OpError, CliError] },
        /// Sign out of a subscription provider. API keys are removed with `auth remove`.
        "auth logout" => { params: AuthLogoutParams, output: AuthLogout,
                           access: [writes, network, destructive], audience: owner, op: super::auth::logout,
                           errors: [OpError, CliError] },
        /// Remove a stored API key. Removing a provider with no key still succeeds.
        "auth remove" => { params: ProviderId, output: AuthRemoved,
                           access: [writes, destructive], audience: owner, op: super::auth::remove,
                           errors: [OpError, CliError] },
        /// Drop the link to another tool's credentials; that tool's own credentials are not touched.
        "auth unlink" => { params: ProviderId, output: AuthRemoved,
                           access: writes, audience: owner, op: super::auth::unlink,
                           errors: [OpError, CliError] },
        /// List the models of the models.dev catalog, optionally filtered by `--filter`.
        "models" => { params: ModelsParams, output: Vec<ModelItem>,
                      access: [reads, network], audience: owner, op: super::auth::models,
                      errors: [CliError] },
        /// List the named inference profiles with their models.
        "inference list" => { params: InferenceListParams, output: Vec<InferenceItem>,
                              access: reads, audience: owner, op: super::inference::list,
                              errors: [OpError, CliError], default: true },
        /// Show one inference profile.
        "inference show" => { params: InferenceRef, output: InferenceItem,
                              access: reads, audience: owner, op: super::inference::show,
                              errors: [OpError, CliError] },
        /// Write a new inference profile: a model and the token count at which compaction begins.
        "inference add" => { params: InferenceAddParams, output: InferenceAdded,
                             access: writes, audience: owner, op: super::inference::add,
                             errors: [OpError, CliError] },
        /// Use an inference profile for a session from its next turn.
        "inference select" => { params: InferenceSelectParams, output: InferenceSelected,
                                access: writes, audience: owner, op: super::inference::select,
                                errors: [OpError, CliError] },
        /// Put a session back on the default inference.
        "inference clear" => { params: InferenceClearParams, output: InferenceSelected,
                               access: writes, audience: owner, op: super::inference::clear,
                               errors: [OpError, CliError] },
        /// List the named modes with their patterns.
        "mode list" => { params: ModeListParams, output: Vec<ModeItem>,
                         access: reads, audience: owner, op: super::mode::list,
                         errors: [OpError, CliError], default: true },
        /// Show one mode.
        "mode show" => { params: ModeRef, output: ModeItem,
                         access: reads, audience: owner, op: super::mode::show,
                         errors: [OpError, CliError] },
        /// Write a new mode from allow and deny patterns, steering text and a description.
        "mode add" => { params: ModeAddParams, output: ModeAdded,
                        access: writes, audience: owner, op: super::mode::add,
                        errors: [OpError, CliError] },
        /// Use a mode for a session from its next turn.
        "mode select" => { params: ModeSelectParams, output: ModeSelected,
                           access: writes, audience: owner, op: super::mode::select,
                           errors: [OpError, CliError] },
        /// Put a session back on the default mode.
        "mode clear" => { params: ModeClearParams, output: ModeSelected,
                          access: writes, audience: owner, op: super::mode::clear,
                          errors: [OpError, CliError] },
        /// Read one config key; a known key that is not set is null, an unknown key is not found.
        "config get" => { params: ConfigKey, output: ConfigEntry,
                          access: reads, audience: owner, op: super::config::get,
                          errors: [OpError, CliError] },
        /// Set one config key, keeping the file's comments and every other key.
        "config set" => { params: ConfigSetParams, output: ConfigEntry,
                          access: writes, audience: owner, op: super::config::set,
                          errors: [OpError, CliError] },
        /// Show the effective config: defaults and gait.jsonc, with the skills and MCP servers found on disk.
        "config show" => { params: ConfigShowParams, output: serde_json::Value,
                           access: reads, audience: owner, op: super::config::show,
                           errors: [OpError, CliError] },
        /// List the running background daemons, or all of them with `--all`.
        "daemons list" => { params: DaemonListParams, output: Vec<DaemonItem>,
                            access: reads, audience: owner, op: super::daemon::list,
                            errors: [OpError, CliError], default: true },
        /// Stop a running daemon by name and record the stop.
        "daemons stop" => { params: DaemonStopParams, output: DaemonStopped,
                            access: [writes, destructive], audience: owner, op: super::daemon::stop,
                            errors: [OpError, CliError] },
        /// Register an MCP server, a skill or a plugin. An MCP server and a skill start disabled.
        "register" => { params: ExtensionRegisterParams, output: ExtensionRegistered,
                        access: writes, audience: owner, op: super::extension::register,
                        errors: [OpError, CliError] },
        /// Remove a registered extension.
        "unregister" => { params: ExtensionUnregisterParams, output: ExtensionUnregistered,
                          access: [writes, destructive], audience: owner, op: super::extension::unregister,
                          errors: [OpError, CliError] },
        /// Switch an extension on in the gait directory's config, for sessions that start later.
        "enable" => { params: ExtensionToggleParams, output: ExtensionToggled,
                      access: writes, audience: owner, op: super::extension::enable,
                      errors: [OpError, CliError] },
        /// Switch an extension off in the gait directory's config.
        "disable" => { params: ExtensionToggleParams, output: ExtensionToggled,
                       access: writes, audience: owner, op: super::extension::disable,
                       errors: [OpError, CliError] },
        /// Load an extension into one session from its next turn.
        "load" => { params: ExtensionLoadParams, output: ExtensionLoaded,
                    access: writes, audience: owner, op: super::extension::load,
                    errors: [OpError, CliError] },
        /// Unload an extension from one session.
        "unload" => { params: ExtensionLoadParams, output: ExtensionLoaded,
                      access: writes, audience: owner, op: super::extension::unload,
                      errors: [OpError, CliError] },
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
    #[cfg(feature = "local")]
    registry.merge(local::decl::registry());
    #[cfg(feature = "workflow")]
    registry.push_extra(workflow_command(), run_workflow);
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

/// `workflow` stays a clap command: its positional run/continue/log/abort
/// grammar is not an op. `--gait-dir` and `--pretty` are the registry's globals.
#[cfg(feature = "workflow")]
fn workflow_command() -> clap::Command {
    clap::Command::new("workflow")
        .about("Run, continue, inspect or abort a workflow (list, run, continue, log, abort, validate)")
        .arg(clap::Arg::new("action").value_name("ACTION"))
        .arg(
            clap::Arg::new("rest")
                .value_name("ARGS")
                .num_args(0..)
                .trailing_var_arg(true)
                .allow_hyphen_values(true),
        )
}

#[cfg(feature = "workflow")]
fn run_workflow<'a>(
    globals: &'a Globals,
    matches: &'a clap::ArgMatches,
) -> futures_util::future::BoxFuture<'a, u8> {
    Box::pin(async move {
        let args = gait_cli::cli::workflow::WorkflowArgs {
            gait_dir: globals.gait_dir.clone(),
            action: matches.get_one::<String>("action").cloned(),
            rest: matches.get_many::<String>("rest").map(|v| v.cloned().collect()).unwrap_or_default(),
            pretty: matches.try_get_one::<bool>("pretty").ok().flatten().copied().unwrap_or(false),
        };
        // The workflow runner holds non-`Send` state across awaits, as the turn does.
        let result = turn::on_turn_thread(move || gait_cli::cli::workflow::cmd_workflow(args)).await;
        match result {
            Ok(code) => code,
            Err(error) => crate::report_code(&error),
        }
    })
}
