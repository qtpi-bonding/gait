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
pub mod extension;
mod hooks;
pub mod inference;
#[cfg(feature = "local")]
pub mod local;
#[cfg(feature = "full")]
pub mod manual;
pub mod mode;
pub mod processes;
pub mod session;
pub mod turn;
pub mod workspace;

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
        Err(coded_error::into_anyhow(gait_ops::OpError::NotFound(
            format!("session '{session}' is not active"),
        )))
    }
}

/// The `ops!` block. It sits in its own module so `registry()` below can add
/// the call hook to the registry it generates.
pub mod decl {
    #[cfg(feature = "full")]
    use super::auth::{AuthAddParams, AuthLogin, AuthLoginParams};
    use super::auth::{
        AuthEntry, AuthLinkParams, AuthListParams, AuthLogout, AuthLogoutParams, AuthRemoved,
        AuthSaved, AuthStatus, ModelItem, ModelsParams, ProviderId,
    };
    use super::config::{ConfigEntry, ConfigKey, ConfigSetParams, ConfigShowParams};
    use super::extension::{
        ExtensionLoadParams, ExtensionLoaded, ExtensionRegisterParams, ExtensionRegistered,
        ExtensionToggleParams, ExtensionToggled, ExtensionUnregisterParams, ExtensionUnregistered,
    };
    use super::inference::{
        InferenceAddParams, InferenceAdded, InferenceClearParams, InferenceItem,
        InferenceListParams, InferenceRef, InferenceSelectParams, InferenceSelected,
    };
    #[cfg(feature = "local")]
    use super::local::{LocalRunner, LocalRunnerParams, LocalSetup, LocalSetupParams};
    use super::mode::{
        ModeAddParams, ModeAdded, ModeClearParams, ModeItem, ModeListParams, ModeRef,
        ModeSelectParams, ModeSelected,
    };
    use super::processes::{
        ProcessListParams, ProcessStopParams, ProcessStopped, TrackedProcessItem,
    };
    use super::session::{
        AllowParams, DiscardParams, PersistParams, PsParams, ReplyParams, SessionId,
        SessionsParams, UndoParams, UnlockParams,
    };
    use super::session::{
        AllowResult, DenyResult, ProcessItem, ReplyResult, SessionAct, SessionItem, SessionStats,
        SessionSummary, UndoResult, UnlockResult,
    };
    use super::workspace::{InitParams, InitResult, LinkParams, LinkResult};
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
            "processes" => "Background processes that sessions started.",
            "local" => "Local models run by llama.cpp: set up a profile and manage its runner.",
        };
        /// List the active sessions, most recently updated first.
        "sessions" => { params: SessionsParams, output: Vec<SessionItem>,
                        access: reads, audience: owner, op: super::session::sessions,
                        errors: [OpError], paths: [cli, typed] },
        /// Show each mounted session and its live state.
        "ps" => { params: PsParams, output: Vec<ProcessItem>,
                  access: reads, audience: owner, op: super::session::ps,
                  errors: [OpError], paths: [cli] },
        /// Show a session's token use, split into agent work and compaction.
        "stats" => { params: SessionId, output: SessionStats,
                     access: reads, audience: owner, op: super::session::stats,
                     errors: [OpError], paths: [cli, typed] },
        /// Show the outcome of a session's last completed run: its message, checkpoint, and the files and tools it touched.
        "log" => { params: SessionId, output: SessionSummary,
                   access: reads, audience: owner, op: super::session::log,
                   errors: [OpError], paths: [cli] },
        /// Go back to an earlier turn of a session, or list the turns with `--list`.
        "undo" => { params: UndoParams, output: UndoResult,
                    access: [writes, destructive], audience: owner, op: super::session::undo,
                    errors: [OpError], paths: [cli] },
        /// Merge a session onto main and remove its worktree, or all idle sessions with `--all --yes`.
        "persist" => { params: PersistParams, output: SessionAct,
                       access: writes, audience: owner, op: super::session::persist,
                       errors: [OpError], paths: [cli] },
        /// Remove a session's worktree without merging; unrecoverable. `--all --yes --force` does it for all idle sessions.
        "discard" => { params: DiscardParams, output: SessionAct,
                       access: [writes, destructive], audience: owner, op: super::session::discard,
                       errors: [OpError], paths: [cli] },
        /// Approve the permission requests a session is waiting on.
        "allow" => { params: AllowParams, output: AllowResult,
                     access: writes, audience: owner, op: super::session::allow,
                     errors: [OpError], paths: [cli] },
        /// Refuse the permission requests a session is waiting on.
        "deny" => { params: SessionId, output: DenyResult,
                    access: writes, audience: owner, op: super::session::deny,
                    errors: [OpError], paths: [cli, typed] },
        /// Approve the sandbox unlock requests a session is waiting on.
        "unlock" => { params: UnlockParams, output: UnlockResult,
                      access: writes, audience: owner, op: super::session::unlock,
                      errors: [OpError], paths: [cli] },
        /// Answer the single clarify or inform question a session is waiting on.
        "reply" => { params: ReplyParams, output: ReplyResult,
                     access: writes, audience: owner, op: super::session::reply,
                     errors: [OpError], paths: [cli] },
        /// List the configured providers and what kind of credential each has; never any key text.
        "auth list" => { params: AuthListParams, output: Vec<AuthEntry>,
                         access: reads, audience: owner, op: super::auth::list,
                         errors: [OpError, CliError], default: true, paths: [cli] },
        /// Show whether a provider has a credential and of what kind, without revealing it.
        "auth status" => { params: ProviderId, output: AuthStatus,
                           access: reads, audience: owner, op: super::auth::status,
                           errors: [OpError, CliError], paths: [cli] },
        #[cfg(feature = "full")]
        /// Store an API key for a provider. Use `--key-file -` for scripts; otherwise the terminal prompts without echo.
        "auth add" => { params: AuthAddParams, output: AuthSaved,
                        access: writes, audience: owner, op: super::auth::add,
                        errors: [OpError, CliError], paths: [cli] },
        /// Use another tool's credentials for a provider without copying them.
        "auth link" => { params: AuthLinkParams, output: AuthSaved,
                         access: writes, audience: owner, op: super::auth::link,
                         errors: [OpError, CliError], paths: [cli] },
        #[cfg(feature = "full")]
        /// Sign in to a subscription provider. For scripts, complete `openai-signin` with `--callback-file -`.
        "auth login" => { params: AuthLoginParams, output: AuthLogin,
                          access: [writes, network], audience: owner, op: super::auth::login,
                          errors: [OpError, CliError], paths: [cli] },
        /// Sign out of a subscription provider. API keys are removed with `auth remove`.
        "auth logout" => { params: AuthLogoutParams, output: AuthLogout,
                           access: [writes, network, destructive], audience: owner, op: super::auth::logout,
                           errors: [OpError, CliError], paths: [cli] },
        /// Remove a stored API key. Removing a provider with no key still succeeds; the id is absent_ok.
        "auth remove" => { params: ProviderId, output: AuthRemoved,
                           access: [writes, destructive], audience: owner, op: super::auth::remove,
                           errors: [OpError, CliError], paths: [cli] },
        /// Drop the link to another tool's credentials; that tool's own credentials are not touched.
        "auth unlink" => { params: ProviderId, output: AuthRemoved,
                           access: writes, audience: owner, op: super::auth::unlink,
                           errors: [OpError, CliError], paths: [cli] },
        /// List the models of the models.dev catalog, optionally filtered by `--filter`.
        "models" => { params: ModelsParams, output: Vec<ModelItem>,
                      access: [reads, network], audience: owner, op: super::auth::models,
                      errors: [CliError], paths: [cli] },
        /// List the named inference profiles with their models.
        "inference list" => { params: InferenceListParams, output: Vec<InferenceItem>,
                              access: reads, audience: owner, op: super::inference::list,
                              errors: [OpError, CliError], default: true, paths: [cli] },
        /// Show one inference profile.
        "inference show" => { params: InferenceRef, output: InferenceItem,
                              access: reads, audience: owner, op: super::inference::show,
                              errors: [OpError, CliError], paths: [cli] },
        /// Write a new inference profile: a model and the token count at which compaction begins.
        "inference add" => { params: InferenceAddParams, output: InferenceAdded,
                             access: writes, audience: owner, op: super::inference::add,
                             errors: [OpError, CliError], paths: [cli] },
        /// Use an inference profile for a session from its next turn.
        "inference select" => { params: InferenceSelectParams, output: InferenceSelected,
                                access: writes, audience: owner, op: super::inference::select,
                                errors: [OpError, CliError], paths: [cli] },
        /// Put a session back on the default inference.
        "inference clear" => { params: InferenceClearParams, output: InferenceSelected,
                               access: writes, audience: owner, op: super::inference::clear,
                               errors: [OpError, CliError], paths: [cli] },
        /// List the named modes with their patterns.
        "mode list" => { params: ModeListParams, output: Vec<ModeItem>,
                         access: reads, audience: owner, op: super::mode::list,
                         errors: [OpError, CliError], default: true, paths: [cli] },
        /// Show one mode.
        "mode show" => { params: ModeRef, output: ModeItem,
                         access: reads, audience: owner, op: super::mode::show,
                         errors: [OpError, CliError], paths: [cli] },
        /// Write a new mode from allow and deny patterns, steering text and a description.
        "mode add" => { params: ModeAddParams, output: ModeAdded,
                        access: writes, audience: owner, op: super::mode::add,
                        errors: [OpError, CliError], paths: [cli] },
        /// Use a mode for a session from its next turn.
        "mode select" => { params: ModeSelectParams, output: ModeSelected,
                           access: writes, audience: owner, op: super::mode::select,
                           errors: [OpError, CliError], paths: [cli] },
        /// Put a session back on the default mode.
        "mode clear" => { params: ModeClearParams, output: ModeSelected,
                          access: writes, audience: owner, op: super::mode::clear,
                          errors: [OpError, CliError], paths: [cli] },
        /// Read one config key; a known key that is not set is null, an unknown key is not found.
        "config get" => { params: ConfigKey, output: ConfigEntry,
                          access: reads, audience: owner, op: super::config::get,
                          errors: [OpError, CliError], paths: [cli] },
        /// Set one config key, keeping the file's comments and every other key.
        "config set" => { params: ConfigSetParams, output: ConfigEntry,
                          access: writes, audience: owner, op: super::config::set,
                          errors: [OpError, CliError], paths: [cli] },
        /// Show the effective config: defaults and gait.jsonc, with the skills and MCP servers found on disk.
        "config show" => { params: ConfigShowParams, output: serde_json::Value,
                           access: reads, audience: owner, op: super::config::show,
                           errors: [OpError, CliError], paths: [cli] },
        /// List the running background processes, or all of them with `--all`.
        "processes list" => { params: ProcessListParams, output: Vec<TrackedProcessItem>,
                            access: reads, audience: owner, op: super::processes::list,
                            errors: [OpError, CliError], default: true, paths: [cli] },
        /// Stop a running process by name and record the stop.
        "processes stop" => { params: ProcessStopParams, output: ProcessStopped,
                            access: [writes, destructive], audience: owner, op: super::processes::stop,
                            errors: [OpError, CliError], paths: [cli] },
        /// Register an MCP server, a skill or a plugin. An MCP server and a skill start disabled.
        "register" => { params: ExtensionRegisterParams, output: ExtensionRegistered,
                        access: writes, audience: owner, op: super::extension::register,
                        errors: [OpError, CliError], paths: [cli] },
        /// Remove a registered extension.
        "unregister" => { params: ExtensionUnregisterParams, output: ExtensionUnregistered,
                          access: [writes, destructive], audience: owner, op: super::extension::unregister,
                          errors: [OpError, CliError], paths: [cli] },
        /// Switch an extension on in the gait directory's config, for sessions that start later.
        "enable" => { params: ExtensionToggleParams, output: ExtensionToggled,
                      access: writes, audience: owner, op: super::extension::enable,
                      errors: [OpError, CliError], paths: [cli] },
        /// Switch an extension off in the gait directory's config.
        "disable" => { params: ExtensionToggleParams, output: ExtensionToggled,
                       access: writes, audience: owner, op: super::extension::disable,
                       errors: [OpError, CliError], paths: [cli] },
        /// Load an extension into one session from its next turn.
        "load" => { params: ExtensionLoadParams, output: ExtensionLoaded,
                    access: writes, audience: owner, op: super::extension::load,
                    errors: [OpError, CliError], paths: [cli] },
        /// Unload an extension from one session.
        "unload" => { params: ExtensionLoadParams, output: ExtensionLoaded,
                      access: writes, audience: owner, op: super::extension::unload,
                      errors: [OpError, CliError], paths: [cli] },
        /// Create a gait directory at `--gait-dir`: config, a default agent, skills and sessions, as a git repository.
        "init" => { params: InitParams, output: InitResult,
                    access: writes, audience: owner, local: super::workspace::init,
                    errors: [OpError, CliError], paths: [cli] },
        /// Point the current directory at the gait directory `--gait-dir` by writing a `.gaitref` file.
        "link" => { params: LinkParams, output: LinkResult,
                    access: writes, audience: owner, local: super::workspace::link,
                    errors: [OpError, CliError], paths: [cli] },
        /// Write a local inference profile for a Qwen2.5-Coder GGUF file; `--id` defaults to `local-qwen`.
        #[cfg(feature = "local")]
        "local setup" => { params: LocalSetupParams, output: LocalSetup,
                           access: writes, audience: owner, op: super::local::setup,
                           errors: [OpError, CliError], paths: [cli] },
        /// Show whether the local model runner for a profile is running; `--id` defaults to `local-qwen`.
        #[cfg(feature = "local")]
        "local status" => { params: LocalRunnerParams, output: LocalRunner,
                            access: reads, audience: owner, op: super::local::status,
                            errors: [OpError, CliError], paths: [cli] },
        /// Stop the local model runner for a profile; `--id` defaults to `local-qwen`.
        #[cfg(feature = "local")]
        "local stop" => { params: LocalRunnerParams, output: LocalRunner,
                          access: writes, audience: owner, op: super::local::stop,
                          errors: [OpError, CliError], paths: [cli] },
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
    // `gait-min` has no `manual install`: the library's `manual` op prints the
    // manual (and `--json` the descriptors, without output schemas).
    #[cfg(not(feature = "full"))]
    {
        let manual = crate::manual::manual(
            &registry,
            crate::manual::ManualFlavor::Min,
            gait_repl::plain::MIN_REPL_COMMANDS,
        )
        .expect("the manual manifest is valid");
        registry.with_manual(manual, op_registry::Audience::Owner);
    }
    registry
}

/// A registry whose turn opens `repl` for `-i`.
pub fn registry_with(repl: Repl) -> op_registry::Registry<GaitContext> {
    let mut registry = decl::registry();
    registry.set_call_hook(std::sync::Arc::new(GaitCallHook));
    // The group of the manual ops cannot be declared conditionally, and the
    // `manual` op of `gait-min` is named like it: they stay a block of their own.
    #[cfg(feature = "full")]
    registry.merge(manual::decl::registry());
    #[cfg(feature = "workflow")]
    registry.push_extra(workflow_command(), run_workflow);
    match repl {
        #[cfg(feature = "rich-tui")]
        Repl::Rich => registry.fallback_free_text(
            "turn",
            turn::TURN_HELP,
            turn::TURN_FLAGS,
            true,
            turn::run_rich,
        ),
        Repl::Plain => registry.fallback_free_text(
            "turn",
            turn::TURN_HELP,
            turn::TURN_FLAGS,
            true,
            turn::run_plain,
        ),
    }
    registry
}

/// Writes `error` as the JSON error line on stderr, records it, and returns its
/// exit code (2 for the usage codes). For a failure the registry's runner does
/// not see, such as the turn's own flags.
/// `workflow` stays a clap command: its positional run/continue/log/abort
/// grammar is not an op. `--gait-dir` and `--pretty` are the registry's globals.
#[cfg(feature = "workflow")]
fn workflow_command() -> clap::Command {
    clap::Command::new("workflow")
        .about("Run, continue, inspect or abort a workflow (list, run, continue, log, abort, validate)")
        .arg(clap::Arg::new("action")
                .value_name("ACTION")
                .help("What to do: list, run, continue, log, abort or validate."))
        .arg(
            clap::Arg::new("rest")
                .value_name("ARGS")
                .help("The workflow id or file and any options the action takes.")
                .num_args(0..)
                .trailing_var_arg(true)
                .allow_hyphen_values(true),
        )
}

#[cfg(feature = "workflow")]
fn run_workflow<'a>(
    globals: &'a Globals,
    matches: &'a clap::ArgMatches,
) -> futures_util::future::BoxFuture<'a, anyhow::Result<u8>> {
    Box::pin(async move {
        let args = gait_cli::cli::workflow::WorkflowArgs {
            gait_dir: globals.gait_dir.clone(),
            action: matches.get_one::<String>("action").cloned(),
            rest: matches
                .get_many::<String>("rest")
                .map(|v| v.cloned().collect())
                .unwrap_or_default(),
            pretty: matches
                .try_get_one::<bool>("pretty")
                .ok()
                .flatten()
                .copied()
                .unwrap_or(false),
        };
        // The workflow runner holds non-`Send` state across awaits, as the turn does.
        let result =
            turn::on_turn_thread(move || gait_cli::cli::workflow::cmd_workflow(args)).await;
        result
    })
}
