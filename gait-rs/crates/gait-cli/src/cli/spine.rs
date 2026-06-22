//! Phase 3 Task 4a: spine process (`gait run`).
//!
//! Thin CLI wrapper around `gait_core::turn::spine::relay_brain_turn`. The
//! spine relay, brain runtime, and hand runtime all live in gait-core.
//! This file parses args, pre-writes input events, resolves the API key,
//! and delegates to the shared relay (same path as the REPL).

use std::path::PathBuf;

use gait_core::port::output::{Diag, TurnSink};

use crate::cli::run::RunArgs;

// ── Spine ─────────────────────────────────────────────────────────────────────

pub async fn cmd_spine(argv: &[String], gait_dir: PathBuf, codebase_path: PathBuf) {
    use clap::Parser;

    let args = RunArgs::parse_from(argv);
    let mut sink = gait_core::port::output::CliSink::stdio(false);

    let session_id = args.session_id.clone().unwrap_or_else(|| {
        let id = format!(
            "s{}",
            &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]
        );
        sink.session_id(&id);
        id
    });

    let mut config = gait_core::config::load_config(&gait_dir);
    for p in &args.allow {
        if !config.allow.contains(p) {
            config.allow.push(p.clone());
        }
    }
    for p in &args.deny {
        if !config.deny.contains(p) {
            config.deny.push(p.clone());
        }
    }
    if let Some(n) = args.max_iter {
        config.max_iter = Some(n);
    }
    if args.sandbox_locked {
        config.sandbox.on = true;
    } else if args.no_sandbox {
        config.sandbox.on = false;
    } else if args.sandbox {
        config.sandbox.on = true;
    }

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
        args.model_override.clone(),
    );
    let model = profile.model.clone();
    let provider_name = match model.split_once('/') {
        Some((p, _)) => p.to_string(),
        None => {
            sink.diagnostic(
                Diag::Error,
                &format!("error: model must be 'provider/model', got: '{}'", model),
            );
            std::process::exit(1);
        }
    };
    let api_key = gait_core::auth::resolve_api_key(&provider_name).unwrap_or_default();

    let repos = gait_core::git::Repos::production();

    // Ensure session worktree exists for pre-writing input events.
    repos
        .traces
        .create_session_worktree(&gait_dir, &session_id)
        .unwrap_or_else(|e| {
            sink.diagnostic(
                Diag::Error,
                &format!("error creating session worktree: {e}"),
            );
            std::process::exit(1);
        });

    let log_path = gait_dir
        .join("sessions")
        .join(&session_id)
        .join("log.jsonl");

    // Expand and pre-write input events (brain runs Resume, sees them on first fold).
    if let Some(task_text) = args.task.clone() {
        let hist = gait_dir
            .join("tmp")
            .join(format!("shell-{}.jsonl", &session_id));
        let events = gait_core::session::ingestor::Ingestor::from_session(&gait_dir, &session_id)
            .events()
            .unwrap_or_default();
        let last_blocks = gait_core::session::retrace::last_turn_blocks(&events);
        let expanded = gait_core::expand::expand_input(
            &task_text,
            &gait_core::expand::ExpandCtx {
                codebase: &codebase_path,
                history: &hist,
                last_blocks: &last_blocks,
            },
        );
        let bodies = gait_core::embed::user_turn_events(&repos, &codebase_path, expanded);
        for body in bodies {
            if let Err(e) = gait_core::proto::wire::append(&log_path, body) {
                sink.diagnostic(Diag::Error, &format!("error appending input event: {e}"));
                std::process::exit(1);
            }
        }
    }

    // Build brain argv: same as spine argv but with --session injected and --task stripped.
    let mut brain_args: Vec<String> = argv.to_vec();
    gait_core::turn::spine::strip_task_from_args(&mut brain_args);
    if !brain_args.iter().any(|a| a == "--session") {
        brain_args.push("--session".into());
        brain_args.push(session_id.clone());
    }

    let exe = std::env::current_exe().unwrap_or_else(|e| {
        sink.diagnostic(Diag::Error, &format!("error: current_exe: {e}"));
        std::process::exit(1);
    });

    // Delegate to the shared spine relay in gait-core (same path as the REPL).
    let sandbox_cfg = config.sandbox.clone();
    let sandbox_on = config.sandbox.on;

    let profile_inputs = gait_core::turn::spine::ProfileInputs {
        agent_name: args.agent_name.clone(),
        model_override: args.model_override.clone(),
        allow: args.allow.clone(),
        deny: args.deny.clone(),
    };

    // Shared slot: holds the current cascade group so the signal task can reap it.
    let signal_slot: gait_core::turn::spine::SignalSlot =
        std::sync::Arc::new(std::sync::Mutex::new(None));

    // Spawn SIGINT/SIGTERM handler: kill current group (if any) then exit.
    {
        use tokio::signal::unix::{signal, SignalKind};
        let slot = signal_slot.clone();
        tokio::spawn(async move {
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
            if let Some(group) = slot.lock().unwrap().clone() {
                let _ = group.kill_all();
            }
            std::process::exit(0);
        });
    }

    let brain_failed = gait_core::turn::spine::relay_brain_turn(
        &brain_args,
        &gait_dir,
        &codebase_path,
        &session_id,
        &api_key,
        &exe,
        &sandbox_cfg,
        sandbox_on,
        &profile_inputs,
        &mut sink,
        args.cascade_group.clone(),
        Some(&signal_slot),
    )
    .await
    .unwrap_or_else(|e| {
        sink.diagnostic(Diag::Error, &format!("spine relay error: {e}"));
        true
    });

    // Clear the slot — turn complete, no live group to kill.
    *signal_slot.lock().unwrap() = None;

    if brain_failed {
        std::process::exit(1);
    }
}

// ── Brain ─────────────────────────────────────────────────────────────────────
// The brain runtime (brain_scope, BrainSink, BrainDriver, run_brain) has been
// moved to gait-core/src/turn/brain.rs. The CLI wrapper (`cmd_brain`) lives in
// args.rs and calls `gait_core::turn::brain::run_brain`.
//
// The duplicated spine relay loop, run_tool_for_spine, error_feedback,
// scope_to_proto, and strip_task_from_args have been removed — the CLI now
// delegates to `gait_core::turn::spine::relay_brain_turn` (same path as the
// REPL), which uses `run_tool_as_spine` in core.
