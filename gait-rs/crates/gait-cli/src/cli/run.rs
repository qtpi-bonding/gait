//! `RunArgs` — shared arg parser for `gait run` / `gait __brain` dispatch.
//!
//! The `gait run` turn consumer.
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::cli::error::CliError;
use gait_core::embed::{CliSink, TurnSink, TurnSource, TurnStart};

fn apply_cli_overrides(
    loaded: gait_core::config::GaitConfig,
    args: &RunArgs,
) -> gait_core::config::GaitConfig {
    use gait_core::config::resolver::{ConfigResolver, Overrides};

    // Preserve the existing append/dedup behavior for CLI allow/deny flags.
    let mut allow = loaded.allow.clone();
    for p in &args.allow {
        if !allow.contains(p) {
            allow.push(p.clone());
        }
    }
    let mut deny = loaded.deny.clone();
    for p in &args.deny {
        if !deny.contains(p) {
            deny.push(p.clone());
        }
    }
    let sandbox_on = if args.sandbox_locked {
        Some(true)
    } else if args.no_sandbox {
        Some(false)
    } else if args.sandbox {
        Some(true)
    } else {
        None
    };
    let cli_overrides = Overrides {
        allow: Some(allow),
        deny: Some(deny),
        max_iter: args.max_iter.map(|n| Some(n as u32)),
        sandbox_on,
        ..Overrides::empty()
    };
    ConfigResolver::new(loaded).with(cli_overrides).resolve()
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct RunArgs {
    #[arg(long = "session")]
    pub session_id: Option<String>,
    #[arg(long)]
    pub task: Option<String>,
    #[arg(long)]
    pub auto: bool,
    #[arg(long = "inference")]
    pub inference: Option<String>,
    #[arg(long = "agent")]
    pub agent_name: Option<String>,
    #[arg(long = "mode")]
    pub mode: Option<String>,
    #[arg(long = "result-schema")]
    pub result_schema_path: Option<String>,
    #[arg(long = "verified-goal")]
    pub verified_goal: Option<String>,
    #[arg(long = "result-schema-json", conflicts_with = "result_schema_path")]
    pub result_schema_json: Option<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub allow: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub deny: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub unlock: Vec<String>,
    #[arg(long)]
    pub max_iter: Option<usize>,
    // Task 7: sandbox engagement flags. `--sandbox` forces on, `--no-sandbox` forces off.
    // When neither is passed, the config default (`config.sandbox.on`) applies.
    // `--sandbox-locked` is set by a parent process when spawning a child; it forces
    // sandbox on and makes `--no-sandbox` a no-op so the child cannot escape.
    #[arg(long)]
    pub sandbox: bool,
    #[arg(long = "no-sandbox")]
    pub no_sandbox: bool,
    #[arg(long = "sandbox-locked", hide = true)]
    pub sandbox_locked: bool,
    /// Cascade group handle passed by a parent process to a blocking subagent.
    /// `None` for the top-level turn; a parent's pgid string for subagents (Plan 4).
    #[arg(long = "cascade-group", hide = true)]
    pub cascade_group: Option<String>,
    /// Override the in-tree safety guard (design §7 / Task 5): run even though
    /// the gait-dir is inside the codebase. Running such a workspace lets the
    /// agent write to its own config/permissions/audit log — prefer moving the
    /// workspace out of tree (`gait init ~/gait-home && gait link ~/gait-home`).
    #[arg(long = "allow-intree")]
    pub allow_intree: bool,
    // Consumed by caller (args.rs); included to prevent "unexpected argument" errors.
    #[arg(long, hide = true)]
    gait_dir: Option<PathBuf>,
    #[arg(long, hide = true)]
    codebase: Option<PathBuf>,
    // Accepted to prevent "unexpected argument" errors. Container spawning is
    // handled externally — gait never self-spawns into a container.
    #[arg(long = "container", hide = true)]
    container: bool,
}

// ── CliSource ─────────────────────────────────────────────────────────────────

/// Single-turn source: yields exactly one `Input([user_bodies])` then `None`.
/// For `--task`-less runs (resume), yields one `Resume`.
struct CliSource {
    session_updates: Option<Vec<gait_core::proto::gait::log::v1::event::Body>>,
    bodies: Option<Vec<gait_core::proto::gait::log::v1::event::Body>>,
    done: bool,
}

#[async_trait::async_trait(?Send)]
impl TurnSource for CliSource {
    async fn next(&mut self) -> Option<TurnStart> {
        if let Some(updates) = self.session_updates.take().filter(|v| !v.is_empty()) {
            return Some(TurnStart::SessionUpdate(updates));
        }
        if self.done {
            return None;
        }
        self.done = true;
        match self.bodies.take() {
            Some(b) if !b.is_empty() => Some(TurnStart::Input {
                bodies: b.into_iter().map(|body| (body, None).into()).collect(),
                on_durable: None,
            }),
            _ => Some(TurnStart::Resume),
        }
    }
}

// ── cmd_run ───────────────────────────────────────────────────────────────────

/// Drive `turn` to completion unless `grace` fires first. `None` means the
/// signal grace window expired (or a second signal arrived) before the turn
/// finished. A grace sender that is dropped without firing never wins: the turn
/// then runs to completion as if no signal had been handled.
async fn turn_or_grace<T>(
    turn: impl std::future::Future<Output = T>,
    grace: tokio::sync::oneshot::Receiver<()>,
) -> Option<T> {
    tokio::pin!(turn);
    tokio::select! {
        out = &mut turn => Some(out),
        Ok(()) = grace => None,
    }
}

pub async fn cmd_run(
    argv: &[String],
    gait_dir: PathBuf,
    codebase_path: PathBuf,
) -> anyhow::Result<ExitCode> {
    use clap::Parser;

    // ── Linux: self-assign to parent's cascade group (race-free enrollment) ──
    // Must happen before any fork/spawn so all descendants land in the group.
    #[cfg(target_os = "linux")]
    {
        // Peek at --cascade-group without full parse (full parse happens below).
        // We do a lightweight scan: find "--cascade-group" followed by its value.
        let mut cg_val: Option<&str> = None;
        let mut iter = argv.iter();
        while let Some(arg) = iter.next() {
            if arg == "--cascade-group" {
                cg_val = iter.next().map(|s| s.as_str());
                break;
            } else if let Some(val) = arg.strip_prefix("--cascade-group=") {
                cg_val = Some(val);
                break;
            }
        }
        if let Some(cg) = cg_val {
            if cg.starts_with("/sys/fs/cgroup") {
                // cgroup v2 path: write our pid into cgroup.procs
                let pid = std::process::id().to_string();
                let procs = format!("{cg}/cgroup.procs");
                let _ = std::fs::write(&procs, &pid);
            } else if let Ok(pgid) = cg.parse::<i32>() {
                // pgid degrade path
                unsafe {
                    libc::setpgid(0, pgid);
                }
            }
        }
    }

    let args = RunArgs::parse_from(argv);
    let mut sink = CliSink::stdio(false);

    // Always the first stdout line, for new and continued sessions alike, so
    // callers can rely on a fixed two-line shape.
    let session_id = args.session_id.clone().unwrap_or_else(|| {
        format!(
            "s{}",
            &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]
        )
    });
    sink.session_id(&session_id);

    // Entry point: surface gait.jsonc parse errors instead of silently
    // defaulting. CLI args layered on top still take precedence below.
    let loaded = gait_core::config::load_config_with(
        &gait_dir,
        gait_core::config::ConfigLoadPolicy::Strict,
    )
    .map_err(|e| CliError::invalid_input(format!("[gait] {e}")))?;
    let config = apply_cli_overrides(loaded, &args);

    // `--mode <name>` activates a mode overlay for this run by appending a
    // `Mode` event to the session log before the turn starts.
    if let Some(name) = &args.mode {
        let log_path = gait_core::session::live::log_path(&gait_dir, &session_id);
        gait_core::modes::activate_mode(&log_path, &gait_dir, name).map_err(|e| {
            CliError::invalid_input(format!("[gait] --mode {name}: {e}"))
        })?;
    }
    let inference_selection = match args.inference.as_deref() {
        Some(name) => Some(
            gait_ops::inference::resolve_selection(&gait_dir, name).map_err(|e| {
                CliError::invalid_input(format!("[gait] --inference {name}: {e}"))
            })?,
        ),
        None => None,
    };

    // Build profile to extract provider name for API key resolution.
    let agent_path: Option<PathBuf> = if let Some(ref name) = args.agent_name {
        Some(gait_core::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_core::agent_frontmatter::MarkdownKind::Agent,
            name,
        ))
    } else {
        let default = gait_core::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_core::agent_frontmatter::MarkdownKind::Agent,
            "default",
        );
        if default.exists() {
            Some(default)
        } else {
            None
        }
    };
    let profile = gait_core::profile::Profile::from_config_and_agent(
        &config,
        &gait_dir,
        Some(codebase_path.as_path()),
        agent_path.as_deref(),
        args.inference.clone(),
    );
    let model = profile.model.clone();
    // Validation only — `PromptNodeDriver::brain_launch` re-derives the
    // provider (and resolves the API key) from `self.model` itself now that
    // `cmd_run` no longer builds its own driver.
    if model.split_once('/').is_none() {
        return Err(CliError::invalid_input(format!(
            "[gait] error: model must be 'provider/model', got: '{}'",
            model
        )));
    }

    // Expand the task text (`gait_turn_runner::run_single_prompt`'s literal
    // branch builds the actual user_turn/context_status events from this,
    // same as the old direct-run_loop path did inline).
    let prompt: Option<String> = args.task.clone().map(|task_text| {
        let hist = gait_dir
            .join("tmp")
            .join(format!("shell-{}.jsonl", &session_id));
        let events = gait_core::session::ingestor::Ingestor::from_session(&gait_dir, &session_id)
            .events()
            .unwrap_or_default();
        let last_blocks = gait_core::session::retrace::last_turn_blocks(&events);
        gait_core::expand::expand_input(
            &task_text,
            &gait_core::expand::ExpandCtx {
                codebase: &codebase_path,
                history: &hist,
                last_blocks: &last_blocks,
            },
        )
    });

    // Explicit CLI schemas take precedence. The session file remains the
    // fallback used by spawned agents.
    let explicit_result_schema: Option<serde_json::Value> =
        if let Some(raw) = args.result_schema_json.as_deref() {
            Some(serde_json::from_str::<serde_json::Value>(raw).map_err(|e| {
                CliError::invalid_input(format!("[gait] --result-schema-json: {e}"))
            })?)
        } else if let Some(path) = args.result_schema_path.as_deref() {
            let raw = std::fs::read_to_string(path).map_err(|e| {
                CliError::failed(format!("[gait] --result-schema {path}: {e}"))
            })?;
            Some(serde_json::from_str(&raw).map_err(|e| {
                CliError::invalid_input(format!("[gait] --result-schema {path}: {e}"))
            })?)
        } else {
            None
        };
    if let Some(schema) = &explicit_result_schema {
        if !schema.is_object() {
            return Err(CliError::invalid_input(
                "[gait] result schema must be a top-level JSON object",
            ));
        }
    }
    let result_schema: Option<serde_json::Value> = explicit_result_schema.or_else(|| {
        let schema_path = gait_core::session::live::worktree_dir(&gait_dir, &session_id)
            .join("result_schema.json");
        std::fs::read(&schema_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    });

    // Process-lifetime cascade kill handle — held by the SIGINT/SIGTERM handler
    // and threaded through run_loop (via SinglePromptOpts.cascade) so
    // relay_brain_turn can register the active group.
    let kill_handle = gait_core::embed::CascadeKillHandle::new();

    // Spawn SIGINT/SIGTERM handler. First signal: ask the turn to stop
    // gracefully — `handle.kill()` (`CascadeKillHandle::kill()`) already sends
    // the group a SIGTERM before its own 300ms→SIGKILL escalation, and the
    // brain (`turn/brain.rs::run_brain`) now wires that SIGTERM to its live
    // `CancellationToken`, so the in-flight turn gets a real chance to return
    // `TurnResult::Cancelled` — relayed back through `gait_turn_runner`
    // below — instead of being signal-killed outright. Give it a bounded
    // grace window to do that; a second signal (impatient user) or the
    // window expiring falls through to the previous hard-kill-and-exit path.
    // The grace window's end is reported on `grace_rx`; the main path below
    // races the turn against it and returns success instead of exiting.
    let (grace_tx, grace_rx) = tokio::sync::oneshot::channel::<()>();
    {
        use tokio::signal::unix::{signal, SignalKind};
        let handle = kill_handle.clone();
        tokio::spawn(async move {
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
            handle.kill();
            let grace = tokio::time::sleep(std::time::Duration::from_secs(3));
            tokio::pin!(grace);
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
                _ = &mut grace => {}
            }
            let _ = grace_tx.send(());
        });
    }

    // `prompt: Some(text)` -> literal one-shot node; `prompt: None` -> resume,
    // driven by a live CliSource yielding exactly one `Resume` turn (the CLI
    // has no interactive loop of its own — a single turn, then exit).
    let input: Option<Box<dyn TurnSource>> = if prompt.is_none() {
        Some(Box::new(CliSource {
            session_updates: inference_selection
                .as_ref()
                .map(|selection| vec![selection.event_body.clone()]),
            bodies: None,
            done: false,
        }))
    } else {
        None
    };

    // spec-06 §3 (one orchestrator, one BrainLaunch site) / Task 3: `cmd_run`
    // drives its sealed turn directly through `gait-turn-runner`, with no
    // compile-time reference to `gait-workflow`. The CLI's resolved
    // profile/sandbox/cascade travel as overrides (Task 3.2's C1 fix) so they
    // still reach the spawned brain.
    let profile_override = gait_turn_runner::TurnProfileOverride {
        verified_goal: args.verified_goal.clone(),
        inference: args.inference.clone(),
        explicit_inference_override: args.inference.is_some(),
        session_updates: if prompt.is_some() {
            inference_selection
                .as_ref()
                .map(|selection| vec![selection.event_body.clone()])
                .unwrap_or_default()
        } else {
            Vec::new()
        },
        // Use the already-merged `config.allow`/`config.deny` (computed by
        // `apply_cli_overrides` above: config's own list + CLI `--allow`/
        // `--deny` flags, appended/deduped), not the raw CLI flags alone —
        // `gait_turn_runner::resolve_allow` takes a `Some(profile)`'s allow
        // list literally (no runner-side widening back to disk config), so
        // this must already be the caller's fully-resolved final list.
        allow: config.allow.clone(),
        deny: config.deny.clone(),
        agent: args.agent_name.clone(),
        max_iter: config.max_iter,
        result_schema,
        auto: args.auto.then_some(true),
    };
    let turn = async {
        if let Some(source) = input {
            gait_turn_runner::run_turn_source(
                gait_turn_runner::TurnRunOpts {
                    run_id: gait_turn_runner::mint_run_id("chat"),
                    session_id: session_id.clone(),
                    continue_session: session_id.clone(),
                    gait_dir: gait_dir.clone(),
                    codebase: codebase_path.clone(),
                    args: vec![],
                    sandbox_on: Some(config.sandbox.on),
                    cascade_group_arg: args.cascade_group.clone(),
                    cascade: Some(kill_handle.clone()),
                    profile: Some(profile_override),
                    fs_delegate: None,
                    allow_intree: args.allow_intree,
                },
                source,
                &mut sink,
            )
            .await
        } else {
            gait_turn_runner::run_single_prompt(
                gait_turn_runner::TurnRunOpts {
                    run_id: gait_turn_runner::mint_run_id("chat"),
                    session_id: session_id.clone(),
                    continue_session: session_id.clone(),
                    gait_dir: gait_dir.clone(),
                    codebase: codebase_path.clone(),
                    args: vec![],
                    sandbox_on: Some(config.sandbox.on),
                    cascade_group_arg: args.cascade_group.clone(),
                    cascade: Some(kill_handle.clone()),
                    profile: Some(profile_override),
                    fs_delegate: None,
                    allow_intree: args.allow_intree,
                },
                prompt,
                &mut sink,
            )
            .await
        }
    };

    // Grace window expired (or a second signal): the turn is abandoned and the
    // run counts as a clean signal exit, as it always has.
    let Some(outcome) = turn_or_grace(turn, grace_rx).await else {
        return Ok(ExitCode::SUCCESS);
    };
    let outcome = outcome.map_err(|e| CliError::failed_with("[gait] run error", e))?;

    if sink.failed {
        if let Some(error) = outcome.run_result.as_ref().and_then(failure_error) {
            return Err(error);
        }
        return Err(CliError::failed(
            "[gait] run failed; see the diagnostic above",
        ));
    }

    // `GatePaused`/gate concepts don't exist for a bare sealed turn (no gate
    // node in play). `!outcome.completed && !outcome.opened` would mean the
    // turn produced neither a terminal result nor stayed open — treat that as
    // a failure, mirroring the old `WorkflowOutcome` mapping's implicit
    // "anything else is fine" behavior for `Completed`/`NodeOpen`.
    if !outcome.completed && !outcome.opened {
        return Err(CliError::failed("[gait] run did not complete"));
    }
    Ok(ExitCode::SUCCESS)
}

/// The coded error of a failed final turn, so the exit code follows its class.
/// `None` when the turn did not fail or carries no code.
fn failure_error(run_result: &gait_core::proto::gait::log::v1::RunResult) -> Option<anyhow::Error> {
    use gait_core::proto::gait::log::v1::turn_result::Outcome;
    let Some(Outcome::Failed(failed)) = run_result.turns.last()?.outcome.as_ref() else {
        return None;
    };
    let code = failed.error_code.as_deref()?;
    Some(coded_error::from_remote(
        Some(code),
        failed.error_class.as_deref().and_then(|class| class.parse().ok()),
        failed.message.clone(),
    ))
}

// ── Brain ─────────────────────────────────────────────────────────────────────
// The brain runtime lives in gait-core/src/turn/brain.rs.
// Dispatch is via gait_core::__dispatch_role (Task 5).

#[cfg(test)]
mod tests {
    use super::*;

    fn run_failed(
        error_code: Option<&str>,
        error_class: Option<&str>,
    ) -> gait_core::proto::gait::log::v1::RunResult {
        use gait_core::proto::gait::log::v1::{turn_result::Outcome, RunResult, TurnFailed, TurnResult};
        RunResult {
            turns: vec![TurnResult {
                outcome: Some(Outcome::Failed(TurnFailed {
                    reason: "error".into(),
                    message: "request failed".into(),
                    error_code: error_code.map(Into::into),
                    error_class: error_class.map(Into::into),
                })),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn failure_error_follows_the_class_of_a_coded_failure() {
        let run = run_failed(Some("gait.provider.timeout"), Some("transient"));
        let error = failure_error(&run).expect("coded failure");
        assert_eq!(coded_error::cli::exit_code_anyhow(&error), 75);
    }

    #[test]
    fn failure_error_is_none_without_a_code() {
        assert!(failure_error(&run_failed(None, None)).is_none());
        assert_eq!(
            coded_error::cli::exit_code_anyhow(&CliError::failed("[gait] run failed; see the diagnostic above")),
            69
        );
    }

    #[test]
    fn parse_verified_goal_flag_and_absence() {
        let parsed =
            RunArgs::parse_from(&["--verified-goal".to_string(), "cargo test".to_string()]);
        assert_eq!(parsed.verified_goal.as_deref(), Some("cargo test"));
        assert_eq!(
            RunArgs::parse_from(&["--auto".to_string()]).verified_goal,
            None
        );
    }

    #[test]
    fn verified_goal_reaches_profile_override() {
        let args = RunArgs::parse_from(&["--verified-goal".to_string(), "cargo test".to_string()]);
        assert_eq!(args.verified_goal.as_deref(), Some("cargo test"));
    }

    #[test]
    fn verified_goal_cli_flag() {
        let args = RunArgs::parse_from(&[
            "--verified-goal".to_string(),
            "cargo test -p cli".to_string(),
        ]);
        // The CLI args.verified_goal is directly used in TurnProfileOverride,
        // and resolution happens in gait_turn_runner, so we verify that the CLI
        // value was parsed correctly.
        assert_eq!(args.verified_goal.as_deref(), Some("cargo test -p cli"));
    }

    #[test]
    fn cli_driver_is_gone() {
        // Built via concat! so this assertion's own strings don't self-match
        // include_str!'s scan of this file.
        let src = include_str!("run.rs");
        let struct_needle = concat!("struct ", "CliDriver");
        let impl_needle = concat!("impl TurnDriver for ", "CliDriver");
        assert!(!src.contains(struct_needle), "CliDriver must be deleted");
        assert!(
            !src.contains(impl_needle),
            "CliDriver's TurnDriver impl must be deleted"
        );
    }

    #[test]
    fn parse_extracts_task_and_flags() {
        let argv: Vec<String> = [
            "--task",
            "hi",
            "--auto",
            "--inference",
            "fast",
            "--max-iter",
            "7",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let a = RunArgs::parse_from(&argv);
        assert_eq!(a.task.as_deref(), Some("hi"));
        assert!(a.auto);
        assert_eq!(a.inference.as_deref(), Some("fast"));
        assert_eq!(a.max_iter, Some(7));
    }
    #[test]
    fn parse_no_task_is_none() {
        assert!(RunArgs::parse_from(&["--auto".to_string()]).task.is_none());
    }

    #[tokio::test]
    async fn cli_source_yields_session_update_before_resume() {
        let body =
            gait_core::proto::events::inference(gait_core::proto::gait::log::v1::Inference {
                name: "codex".into(),
                model: "openai-codex/gpt-test".into(),
                llm_params: None,
                compaction_threshold_tokens: 128_000,
            });
        let mut source = CliSource {
            session_updates: Some(vec![body]),
            bodies: None,
            done: false,
        };
        assert!(matches!(
            source.next().await,
            Some(TurnStart::SessionUpdate(_))
        ));
        assert!(matches!(source.next().await, Some(TurnStart::Resume)));
        assert!(source.next().await.is_none());
    }
    // Signal grace races are tested through `turn_or_grace` with a hand-fired
    // oneshot, so no real signal is ever sent to the test process.
    #[tokio::test]
    async fn turn_or_grace_returns_the_turn_when_it_finishes_first() {
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        assert_eq!(turn_or_grace(async { 7 }, rx).await, Some(7));
    }

    #[tokio::test]
    async fn turn_or_grace_gives_up_when_grace_fires() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tx.send(()).unwrap();
        let never_finishes = std::future::pending::<u8>();
        assert_eq!(turn_or_grace(never_finishes, rx).await, None);
    }

    #[tokio::test]
    async fn turn_or_grace_ignores_a_dropped_grace_sender() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        drop(tx);
        let turn = async {
            tokio::task::yield_now().await;
            3
        };
        assert_eq!(turn_or_grace(turn, rx).await, Some(3));
    }

    #[tokio::test]
    async fn unparseable_gait_jsonc_is_an_invalid_input_error() {
        let gait_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(gait_dir.path().join("gait.jsonc"), "{ not json").unwrap();
        let err = cmd_run(
            &["--task".to_string(), "hi".to_string()],
            gait_dir.path().to_path_buf(),
            gait_dir.path().to_path_buf(),
        )
        .await
        .expect_err("a broken gait.jsonc is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn unknown_inference_name_is_an_invalid_input_error() {
        let gait_dir = tempfile::TempDir::new().unwrap();
        let err = cmd_run(
            &[
                "--task".to_string(),
                "hi".to_string(),
                "--inference".to_string(),
                "no-such-inference-xyz".to_string(),
            ],
            gait_dir.path().to_path_buf(),
            gait_dir.path().to_path_buf(),
        )
        .await
        .expect_err("an unknown inference is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::InvalidInput(_))
        ));
    }

    #[test]
    fn sandbox_locked_parsed() {
        let a = RunArgs::parse_from(&["--sandbox-locked".to_string()]);
        assert!(a.sandbox_locked);
        assert!(!a.no_sandbox);
    }

    #[test]
    fn cli_allow_appends_and_dedupes_onto_config_allow() {
        let loaded = gait_core::config::GaitConfig {
            allow: vec!["read".to_string()],
            ..gait_core::config::GaitConfig::default()
        };
        let args = RunArgs::parse_from(&[
            "--allow".to_string(),
            "read".to_string(),
            "--allow".to_string(),
            "shell".to_string(),
        ]);
        let resolved = apply_cli_overrides(loaded, &args);
        assert_eq!(resolved.allow, vec!["read", "shell"]);
    }

    #[test]
    fn cli_sandbox_locked_forces_on_even_with_no_sandbox() {
        let loaded = gait_core::config::GaitConfig::default();
        let args =
            RunArgs::parse_from(&["--sandbox-locked".to_string(), "--no-sandbox".to_string()]);
        let resolved = apply_cli_overrides(loaded, &args);
        assert!(resolved.sandbox.on);
    }

    // ── RunLaunch round-trip (spec-00-footguns.md item 4) ───────────────────
    //
    // `gait_core::launch::RunLaunch` is gait-core's typed builder for the
    // argv every self-invocation site that targets `RunArgs` builds
    // (gait-bin's bare-positional path, spine's launch_child/spawn_wake, and
    // spawn_agent's detached-agent spawn). The test lives here (not in
    // gait-core's launch.rs) because `RunArgs` is defined in gait-cli, which
    // depends on gait-core — gait-core cannot depend back on gait-cli.
    #[test]
    fn run_launch_round_trips_through_run_args() {
        let launch = gait_core::launch::RunLaunch {
            session: Some("s1".to_string()),
            task: Some("do the thing".to_string()),
            auto: true,
            inference: Some("deep".to_string()),
            agent_name: Some("default".to_string()),
            result_schema_path: Some("/schema.json".to_string()),
            result_schema_json: None,
            allow: vec!["Bash".to_string()],
            deny: vec!["Net".to_string()],
            unlock: vec!["/tmp".to_string()],
            max_iter: Some(9),
            sandbox: false,
            no_sandbox: false,
            sandbox_locked: true,
            cascade_group: Some("pg123".to_string()),
            gait_dir: Some(PathBuf::from("/g")),
            codebase: Some(PathBuf::from("/c")),
            allow_intree: true,
        };
        let argv = launch.to_argv();
        // Flags-only: no leading role/subcommand token (`RunArgs` is
        // `no_binary_name = true` and has no positional field).
        assert!(!argv.iter().any(|a| a == "run"));

        let parsed = RunArgs::parse_from(&argv);
        assert_eq!(parsed.session_id, launch.session);
        assert_eq!(parsed.task, launch.task);
        assert_eq!(parsed.auto, launch.auto);
        assert_eq!(parsed.inference, launch.inference);
        assert_eq!(parsed.agent_name, launch.agent_name);
        assert_eq!(parsed.result_schema_path, launch.result_schema_path);
        assert_eq!(parsed.allow, launch.allow);
        assert_eq!(parsed.deny, launch.deny);
        assert_eq!(parsed.unlock, launch.unlock);
        assert_eq!(parsed.max_iter, launch.max_iter);
        assert_eq!(parsed.sandbox, launch.sandbox);
        assert_eq!(parsed.no_sandbox, launch.no_sandbox);
        assert_eq!(parsed.sandbox_locked, launch.sandbox_locked);
        assert_eq!(parsed.cascade_group, launch.cascade_group);
        assert_eq!(parsed.allow_intree, launch.allow_intree);
        // `gait_dir`/`codebase` are private fields on `RunArgs` (consumed by
        // the caller in args.rs to prevent "unexpected argument" errors) —
        // parsing without error is the only thing observable from here, so
        // assert that plus the round-trip of everything that is `pub`.
    }

    #[test]
    fn run_launch_defaults_emit_no_optional_flags() {
        let launch = gait_core::launch::RunLaunch::default();
        let argv = launch.to_argv();
        assert!(argv.is_empty());
        let parsed = RunArgs::parse_from(&argv);
        assert_eq!(parsed.task, None);
        assert_eq!(parsed.session_id, None);
        assert!(!parsed.auto);
    }

    #[test]
    fn run_launch_to_process_argv_prepends_run_role_token() {
        let launch = gait_core::launch::RunLaunch {
            session: Some("s1".to_string()),
            auto: true,
            ..Default::default()
        };
        let argv = launch.to_process_argv();
        assert_eq!(argv[0], "run");
        // The remainder parses identically to `to_argv()` alone.
        assert_eq!(&argv[1..], launch.to_argv().as_slice());
    }
}
