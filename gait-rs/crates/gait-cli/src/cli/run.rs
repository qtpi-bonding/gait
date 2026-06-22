//! gait run command (extracted from bin/gait.rs).
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct RunArgs {
    #[arg(long = "session")]
    pub session_id: Option<String>,
    #[arg(long)]
    pub task: Option<String>,
    #[arg(long)]
    pub auto: bool,
    #[arg(long = "model")]
    pub model_override: Option<String>,
    #[arg(long = "agent")]
    pub agent_name: Option<String>,
    #[arg(long = "result-schema")]
    pub result_schema_path: Option<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub allow: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub deny: Vec<String>,
    #[arg(long)]
    pub max_iter: Option<usize>,
    // Task 7: sandbox engagement flags. `--sandbox` forces on, `--no-sandbox` forces off.
    // When neither is passed, the config default (`config.sandbox.on`) applies.
    // `--sandbox-locked` is set by a parent spine when spawning a child; it forces
    // sandbox on and makes `--no-sandbox` a no-op so the child cannot escape.
    #[arg(long)]
    pub sandbox: bool,
    #[arg(long = "no-sandbox")]
    pub no_sandbox: bool,
    #[arg(long = "sandbox-locked", hide = true)]
    pub sandbox_locked: bool,
    /// Cascade group handle passed by a parent spine to a blocking subagent.
    /// `None` for the top-level turn; a parent's pgid string for subagents (Plan 4).
    #[arg(long = "cascade-group", hide = true)]
    pub cascade_group: Option<String>,
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

/// `gait run` — one turn, streaming to stdout. The bin owns path resolution and
/// passes the resolved `gait_dir` + `codebase_path` in; flags are parsed via
/// [`RunArgs`].
pub async fn cmd_run(argv: &[String], gait_dir: PathBuf, codebase_path: PathBuf) {
    use gait_core::port::output::{Diag, TurnSink};
    let args = RunArgs::parse_from(argv);

    // The single owned output boundary: Trace+Error stream to stderr, the final
    // RunResult prints once to stdout, diagnostics go to stderr (and set `failed`).
    let mut sink = gait_core::port::output::CliSink::stdio(false);

    // A fresh session announces its id on stdout (proto SessionStart). `--session`
    // resumes silently (the closure — and thus this line — never runs).
    let session_id = args.session_id.unwrap_or_else(|| {
        let id = format!(
            "s{}",
            &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]
        );
        sink.session_id(&id);
        id
    });

    let mut config = gait_core::config::load_config(&gait_dir);

    // Merge CLI --allow / --deny (additive: CLI entries extend, never replace, gait.jsonc lists).
    for p in args.allow {
        if !config.allow.contains(&p) {
            config.allow.push(p);
        }
    }
    for p in args.deny {
        if !config.deny.contains(&p) {
            config.deny.push(p);
        }
    }
    if let Some(n) = args.max_iter {
        config.max_iter = Some(n);
    }
    // Resolve sandbox engagement. --sandbox-locked (set by a parent spine) forces sandbox
    // on and ignores --no-sandbox so spawned children cannot escape the parent's policy.
    // Without --sandbox-locked, --no-sandbox wins over --sandbox (off-first semantics).
    if args.sandbox_locked {
        config.sandbox.on = true;
    } else if args.no_sandbox {
        config.sandbox.on = false;
    } else if args.sandbox {
        config.sandbox.on = true;
    }

    // Apply --result-schema override: read file, validate JSON Schema, error early if invalid.
    if let Some(ref path) = args.result_schema_path {
        let raw = std::fs::read_to_string(path).unwrap_or_else(|e| {
            sink.diagnostic(
                Diag::Error,
                &format!("--result-schema: cannot read '{}': {}", path, e),
            );
            std::process::exit(1);
        });
        let schema: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
            sink.diagnostic(
                Diag::Error,
                &format!("--result-schema: '{}' is not valid JSON: {}", path, e),
            );
            std::process::exit(1);
        });
        jsonschema::JSONSchema::compile(&schema).unwrap_or_else(|e| {
            sink.diagnostic(
                Diag::Error,
                &format!(
                    "--result-schema: '{}' is not a valid JSON Schema: {}",
                    path, e
                ),
            );
            std::process::exit(1);
        });
        config.result_schema = Some(schema);
    }

    // Resolve agent file: explicit --agent name, or fallback to agents/default/AGENT.md
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

    // Build the declarative profile (model precedence: --model > agent frontmatter > config),
    // then resolve it into a live provider + tools + MCP clients in one place.
    let profile = gait_core::profile::Profile::from_config_and_agent(
        &config,
        &gait_dir,
        Some(codebase_path.as_path()),
        agent_path.as_deref(),
        args.model_override.clone(),
    );
    let model = profile.model.clone();

    let provider_name = match model.split_once('/') {
        Some((p, _)) => p,
        None => {
            sink.diagnostic(
                Diag::Error,
                &format!("error: model must be 'provider/model', got: '{}'", model),
            );
            std::process::exit(1);
        }
    };
    let api_key = gait_core::auth::resolve_api_key(provider_name);

    let repos = gait_core::git::Repos::production();

    // (Profile is resolved per-turn inside the CliDriver, below — the loop owns
    // that now. A resolve error surfaces as the run_loop's Err.)

    // Expand $(cmd)/$(N) shell, @path files, and >(N) quote-reply in the task
    // string before the turn runs — same expander the REPL uses. Quote-reply
    // pulls the prior turn's blocks from the session log (empty for a fresh one).
    let task = args.task.map(|t| {
        let hist = gait_dir
            .join("tmp")
            .join(format!("shell-{}.jsonl", &session_id));
        let events = gait_core::session::ingestor::Ingestor::from_session(&gait_dir, &session_id)
            .events()
            .unwrap_or_default();
        let last_blocks = gait_core::session::retrace::last_turn_blocks(&events);
        gait_core::expand::expand_input(
            &t,
            &gait_core::expand::ExpandCtx {
                codebase: &codebase_path,
                history: &hist,
                last_blocks: &last_blocks,
            },
        )
    });

    // The primary input becomes events the loop appends before the fold (or a
    // bare Resume when there's no --task) — symmetric with the reply path.
    // `user_turn_events` returns [user_text, context_status] so the codebase
    // head hash is captured in the log alongside the user prompt. ArgSource
    // wraps a single Body; we hand it the first event (the UserText).
    let source = gait_core::port::input::ArgSource::new(task.map(|t| {
        let mut bodies = gait_core::embed::user_turn_events(&repos, &codebase_path, t);
        bodies.remove(0)
    }));
    let session = std::sync::Arc::new(
        gait_core::session::Session::load(
            gait_dir.clone(),
            session_id.clone(),
            codebase_path.clone(),
            repos.clone(),
        )
        .expect("failed to load session.json"),
    );
    let mut driver = CliDriver {
        profile: Some(profile),
        codebase: codebase_path,
        session_id: session_id.clone(),
        gait_dir: gait_dir.clone(),
        repos,
        api_key,
        auto: args.auto,
        max_iter: config.max_iter,
        preview_policy: config.preview,
        sandbox_on: config.sandbox.on,
        sandbox_cfg: config.sandbox.clone(),
    };

    if let Err(e) = gait_core::embed::run_loop(
        source,
        &mut sink,
        session,
        &gait_dir,
        &session_id,
        &mut driver,
    )
    .await
    {
        sink.diagnostic(Diag::Error, &format!("error: {}", e));
        std::process::exit(1);
    }

    if sink.failed {
        std::process::exit(1);
    }
}

/// CLI driver for `run_loop`: single-shot profile resolve, `--auto`/`--max-iter`
/// config, and the after-turn Scheduler/Harness coordination. A turn error sets
/// `failed` (via `diagnostic`) and breaks the loop → the post-loop `exit(1)`.
struct CliDriver {
    profile: Option<gait_core::profile::Profile>,
    codebase: PathBuf,
    session_id: String,
    gait_dir: PathBuf,
    repos: gait_core::git::Repos,
    api_key: Option<String>,
    auto: bool,
    max_iter: Option<usize>,
    preview_policy: gait_core::config::PreviewPolicy,
    sandbox_on: bool,
    sandbox_cfg: gait_core::config::SandboxConfig,
}

#[async_trait::async_trait(?Send)]
impl gait_core::embed::TurnDriver for CliDriver {
    async fn resolve_profile(&mut self) -> anyhow::Result<gait_core::profile::ResolvedProfile> {
        // Resolves a profile pre-built in cmd_run from flag-merged config
        // (--allow/--deny/--max-iter/--result-schema), so it can't use the plain
        // `embed::resolve_profile`. Single-shot: `ArgSource` yields one turn → resolves once.
        let profile = self.profile.take().ok_or_else(|| {
            anyhow::anyhow!("CLI driver resolve_profile() called more than once (single-shot)")
        })?;
        let runner = std::sync::Arc::new(gait_workflow::GaitWorkflowRunner::new(
            self.gait_dir.clone(),
            self.codebase.clone(),
        ));
        profile
            .resolve(
                self.codebase.clone(),
                self.session_id.clone(),
                self.gait_dir.clone(),
                self.repos.clone(),
                self.api_key.clone(),
                Some(runner),
            )
            .await
    }

    fn turn_config(&mut self) -> gait_core::embed::LoopConfig {
        gait_core::embed::LoopConfig {
            auto: self.auto,
            max_iter: self.max_iter,
            preview_policy: self.preview_policy,
            cancel: None,
            sandbox_on: self.sandbox_on,
            sandbox_cfg: self.sandbox_cfg.clone(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        }
    }

    async fn after_turn(
        &mut self,
        sink: &mut dyn gait_core::port::output::TurnSink,
        result: &gait_core::turn::TurnResult,
    ) {
        use gait_core::{HarnessBackend, Scheduler};
        let meta = gait_core::session::read_session_meta(&self.gait_dir, &self.session_id)
            .unwrap_or_else(|e| {
                eprintln!("[gait] warning: could not read session meta in after_turn: {e}");
                gait_core::session::SessionMeta {
                    id: self.session_id.clone(),
                    ..Default::default()
                }
            });
        let scheduler = gait_core::scheduler::NativeScheduler;
        let harness = gait_core::harness::NativeHarnessBackend::new(self.gait_dir.clone());
        let ended = gait_core::scheduler::TurnEnded {
            session_id: &self.session_id,
            assigned_by: meta.assigned_by.as_deref(),
            result,
        };
        for action in scheduler.next(&ended) {
            if let Err(e) = harness.execute(&action).await {
                sink.diagnostic(
                    gait_core::port::output::Diag::Info,
                    &format!("warning: harness execute failed: {}", e),
                );
            }
        }
    }

    fn on_turn_error(
        &mut self,
        sink: &mut dyn gait_core::port::output::TurnSink,
        e: anyhow::Error,
    ) -> std::ops::ControlFlow<()> {
        sink.diagnostic(
            gait_core::port::output::Diag::Error,
            &format!("error: {}", e),
        );
        std::ops::ControlFlow::Break(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_extracts_task_and_flags() {
        let argv: Vec<String> = [
            "--task",
            "hi",
            "--auto",
            "--model",
            "x/y",
            "--max-iter",
            "7",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let a = RunArgs::parse_from(&argv);
        assert_eq!(a.task.as_deref(), Some("hi"));
        assert!(a.auto);
        assert_eq!(a.model_override.as_deref(), Some("x/y"));
        assert_eq!(a.max_iter, Some(7));
    }
    #[test]
    fn parse_no_task_is_none() {
        assert!(RunArgs::parse_from(&["--auto".to_string()]).task.is_none());
    }
    #[test]
    fn sandbox_locked_parsed() {
        let a = RunArgs::parse_from(&["--sandbox-locked".to_string()]);
        assert!(a.sandbox_locked);
        assert!(!a.no_sandbox);
    }
}
