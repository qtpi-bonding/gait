//! The turn: `gait "task"`, `gait -i`, `gait -e "prompt"`. It is the registry's
//! free-text fallback: everything that is not a command, after the registry's
//! globals, arrives here as raw arguments, which this module parses with its
//! own flags (the shorts `-i -e -s -a -n -R -c` belong to the turn).
use super::{GaitContext, Globals};
use clap::{ArgAction, Parser};
use futures_util::future::BoxFuture;
use op_registry::cli::FallbackFlag;
use std::ffi::OsString;
use std::path::PathBuf;

/// The long flags the turn reads itself (the registry checks they do not
/// reuse a global's name).
pub const TURN_FLAGS: &[FallbackFlag] = &[
    FallbackFlag {
        long: "session",
        short: Some('s'),
        takes_value: true,
    },
    FallbackFlag {
        long: "inference",
        short: None,
        takes_value: true,
    },
    FallbackFlag {
        long: "verified-goal",
        short: None,
        takes_value: true,
    },
    FallbackFlag {
        long: "agent",
        short: Some('a'),
        takes_value: true,
    },
    FallbackFlag {
        long: "auto-approve",
        short: None,
        takes_value: false,
    },
    FallbackFlag {
        long: "allow",
        short: None,
        takes_value: true,
    },
    FallbackFlag {
        long: "deny",
        short: None,
        takes_value: true,
    },
    FallbackFlag {
        long: "unlock",
        short: None,
        takes_value: true,
    },
    FallbackFlag {
        long: "max-iter",
        short: Some('n'),
        takes_value: true,
    },
    FallbackFlag {
        long: "result-schema",
        short: Some('R'),
        takes_value: true,
    },
    FallbackFlag {
        long: "result-schema-json",
        short: None,
        takes_value: true,
    },
    FallbackFlag {
        long: "codebase",
        short: Some('c'),
        takes_value: true,
    },
    FallbackFlag {
        long: "allow-intree",
        short: None,
        takes_value: false,
    },
    FallbackFlag {
        long: "interactive",
        short: Some('i'),
        takes_value: false,
    },
    FallbackFlag {
        long: "exec",
        short: Some('e'),
        takes_value: true,
    },
];

/// What the turn's `-i` runs: the themed REPL of full `gait`, or the plain REPL
/// of `gait-min`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repl {
    #[cfg(feature = "rich-tui")]
    Rich,
    Plain,
}

/// The one-line help the root help lists for the fallback, then its detail.
pub const TURN_HELP: &str =
    "Run a task: `{bin} \"fix the login bug\"`, `-i` for the REPL, `-e` for a one-off request.\n\
A first word that is a command runs the command; put `--` before a task that starts with one.";

/// The turn's own flags. The registry hands over every argument after the
/// leading globals, so a global given after the task (`gait "x" --gait-dir D`)
/// is read here too.
#[derive(Parser, Debug, Clone, Default)]
#[command(name = "gait", disable_help_flag = true, disable_version_flag = true)]
pub struct TurnArgs {
    /// Task to run (bare positional: `gait "fix the login bug"`)
    #[arg(conflicts_with_all = ["interactive", "exec"])]
    pub task: Option<String>,

    /// Interactive REPL mode
    #[arg(short = 'i', long, conflicts_with_all = ["exec", "task"])]
    pub interactive: bool,
    /// Ephemeral one-off request
    #[arg(short = 'e', long, conflicts_with_all = ["interactive", "task"])]
    pub exec: Option<String>,

    /// Continue an existing session by id
    #[arg(long, short = 's')]
    pub session: Option<String>,
    /// Named inference selection.
    #[arg(long)]
    pub inference: Option<String>,
    /// Verified goal: fixed command run when a turn attempts completion to verify its goal.
    #[arg(long)]
    pub verified_goal: Option<String>,
    /// Agent file to load
    #[arg(long, short = 'a')]
    pub agent: Option<String>,
    /// auto-approve all tool-permission prompts; the sandbox ceiling still applies (kernel-enforced)
    #[arg(long)]
    pub auto_approve: bool,
    /// Pre-approve tool pattern (repeatable)
    #[arg(long, action = ArgAction::Append)]
    pub allow: Vec<String>,
    /// Pre-deny tool pattern (repeatable)
    #[arg(long, action = ArgAction::Append)]
    pub deny: Vec<String>,
    /// Pre-grant sandbox path access (repeatable)
    #[arg(long, action = ArgAction::Append)]
    pub unlock: Vec<String>,
    /// Cap agentic steps
    #[arg(long, short = 'n')]
    pub max_iter: Option<usize>,
    /// Path to structured output schema
    #[arg(long, short = 'R')]
    pub result_schema: Option<PathBuf>,
    /// Inline JSON structured output schema
    #[arg(long, conflicts_with = "result_schema")]
    pub result_schema_json: Option<String>,
    #[arg(long, short = 'c', hide = true)]
    pub codebase: Option<PathBuf>,
    /// Override the in-tree safety guard (design §7): run even though the
    /// gait-dir is inside the codebase.
    #[arg(long)]
    pub allow_intree: bool,
}

/// What a parsed command line asks the turn to do. `plan` is the seam the
/// tests use to see the choice without running anything.
#[derive(Debug, PartialEq)]
pub enum TurnPlan {
    /// `-e`: the one-off request.
    Exec { prompt: String },
    /// `-i`: the REPL.
    Interactive,
    /// A task.
    Task { task: String },
    /// Flags but no task: the root help.
    NoTask,
}

impl TurnArgs {
    /// Parses the arguments the fallback received (no program name).
    pub fn parse(args: &[OsString]) -> Result<TurnArgs, clap::Error> {
        TurnArgs::try_parse_from(
            std::iter::once(OsString::from("gait")).chain(args.iter().cloned()),
        )
    }

    pub fn plan(&self) -> TurnPlan {
        if let Some(prompt) = &self.exec {
            TurnPlan::Exec {
                prompt: prompt.clone(),
            }
        } else if self.interactive {
            TurnPlan::Interactive
        } else if let Some(task) = &self.task {
            TurnPlan::Task { task: task.clone() }
        } else {
            TurnPlan::NoTask
        }
    }
}

/// The fallback of the full `gait`: `-i` opens the themed REPL.
#[cfg(feature = "rich-tui")]
pub fn run_rich<'a>(
    globals: &'a Globals,
    args: Vec<OsString>,
) -> BoxFuture<'a, anyhow::Result<u8>> {
    Box::pin(run(Repl::Rich, globals, args))
}

/// The fallback of `gait-min`: `-i` opens the plain REPL.
pub fn run_plain<'a>(
    globals: &'a Globals,
    args: Vec<OsString>,
) -> BoxFuture<'a, anyhow::Result<u8>> {
    Box::pin(run(Repl::Plain, globals, args))
}

async fn run(repl: Repl, globals: &Globals, args: Vec<OsString>) -> anyhow::Result<u8> {
    let parsed = match TurnArgs::parse(&args) {
        Ok(parsed) => parsed,
        Err(error) => {
            let failure = op_registry::UsageError::InvalidField {
                name: "arguments".to_owned(),
                reason: error.to_string(),
            };
            return Err(coded_error::into_anyhow(failure));
        }
    };
    let globals = globals.clone();
    on_turn_thread(move || traced(execute(repl, globals, parsed))).await
}

/// The turn runs inside the `gait.cli` span, parented on `TRACEPARENT`/`BAGGAGE`.
async fn traced(turn: impl std::future::Future<Output = anyhow::Result<u8>>) -> anyhow::Result<u8> {
    #[cfg(feature = "telemetry")]
    {
        use tracing::Instrument;
        let span = tracing::info_span!("gait.cli", op = "turn");
        concord_telemetry::propagation::set_parent_from_env(&span);
        return turn.instrument(span).await;
    }
    #[cfg(not(feature = "telemetry"))]
    turn.await
}

async fn execute(repl: Repl, globals: Globals, cli: TurnArgs) -> anyhow::Result<u8> {
    let plan = cli.plan();
    match plan {
        TurnPlan::NoTask => {
            print!("{}", root_help(repl));
            return Ok(0);
        }
        _ => {}
    }
    let gait_dir = <GaitContext as op_registry::CliContext>::from_globals(&Globals {
        gait_dir: globals.gait_dir,
        identity_dir: globals.identity_dir,
    })?
    .gait_dir()?;
    let codebase = cli
        .codebase
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    match plan {
        TurnPlan::Exec { prompt } => {
            let args = gait_cli::cli::exec::ExecArgs {
                inference: cli.inference,
                result_schema: cli.result_schema,
                result_schema_json: cli.result_schema_json,
                prompt: vec![prompt],
            };
            gait_cli::cli::exec::cmd_exec(args, &gait_dir, &codebase).await
        }
        TurnPlan::Interactive => match repl {
            #[cfg(feature = "rich-tui")]
            Repl::Rich => {
                gait_repl::chat::run_chat(
                    gait_dir,
                    codebase,
                    cli.agent,
                    cli.session,
                    cli.inference,
                    cli.verified_goal,
                )
                .await?;
                Ok(0)
            }
            Repl::Plain => {
                let args = gait_repl::plain::PlainReplArgs {
                    gait_dir,
                    codebase,
                    agent_name: cli.agent,
                    session_id: cli.session,
                    model_override: cli.inference,
                    verified_goal: cli.verified_goal,
                };
                gait_repl::plain::run_plain_repl(args).await?;
                Ok(0)
            }
        },
        TurnPlan::Task { task } => {
            let run_argv = gait_core::launch::RunLaunch {
                task: Some(task),
                session: cli.session,
                auto: cli.auto_approve,
                inference: cli.inference,
                agent_name: cli.agent,
                allow: cli.allow,
                deny: cli.deny,
                unlock: cli.unlock,
                max_iter: cli.max_iter,
                result_schema_path: cli
                    .result_schema
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                result_schema_json: cli.result_schema_json,
                gait_dir: Some(gait_dir.clone()),
                codebase: Some(codebase.clone()),
                allow_intree: cli.allow_intree,
                ..Default::default()
            }
            .to_argv();
            gait_cli::cli::run::cmd_run(&run_argv, gait_dir, codebase).await
        }
        TurnPlan::NoTask => unreachable!("handled above"),
    }
}

/// The registry's root help, as `gait --help` prints it.
fn root_help(repl: Repl) -> String {
    let registry = super::registry_with(repl);
    let mut command = op_registry::cli::command(&registry);
    format!("{}\n", command.render_help())
}

/// Runs the turn to completion on a blocking thread. The turn and the REPLs
/// hold non-`Send` state across awaits, and the registry's fallback future must
/// be `Send`: the turn future is built and driven on that one thread.
pub(super) async fn on_turn_thread<F, Fut>(make: F) -> anyhow::Result<u8>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<u8>>,
{
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(make()))
        .await
        .map_err(|error| gait_cli::cli::error::CliError::failed(format!("turn thread: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::{TurnArgs, TURN_FLAGS};
    use clap::{ArgAction, CommandFactory};

    #[test]
    fn fallback_flags_match_turn_arguments() {
        let command = TurnArgs::command();
        for flag in TURN_FLAGS {
            let argument = command
                .get_arguments()
                .find(|argument| argument.get_long() == Some(flag.long))
                .unwrap_or_else(|| panic!("TurnArgs is missing --{}", flag.long));

            assert_eq!(argument.get_short(), flag.short, "--{} short", flag.long);
            assert_eq!(
                matches!(argument.get_action(), ArgAction::Set | ArgAction::Append),
                flag.takes_value,
                "--{} takes_value",
                flag.long
            );
        }
        // And the other way: no flag of the turn is missing from the table.
        for argument in command.get_arguments() {
            if let Some(long) = argument.get_long() {
                assert!(
                    TURN_FLAGS.iter().any(|flag| flag.long == long),
                    "TURN_FLAGS is missing --{long}"
                );
            }
        }
    }
}
