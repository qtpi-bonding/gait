//! Phase 3 Task 4a: spine process (`gait run`).
//!
//! Spine: resolves API key, pre-writes input events, spawns brain, relay loop.
//! Brain runtime lives in `gait-core::turn::brain`; this file owns only the
//! CLI spine entry point (`cmd_spine`) and the per-call hand launcher
//! (`run_tool_for_spine`).

use std::path::PathBuf;
use std::sync::Arc;

use prost::Message;
use tokio::process::Command;
use tokio::sync::Mutex;

use gait_core::port::output::{Diag, TurnSink};
use gait_core::turn::nerve;
use gait_proto::gait::sandbox::v1::{network_scope::Mode, signal, NetworkScope, Scope, Signal};

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

    // Ensure session worktree exists — brain runs Resume mode (no worktree creation).
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
    strip_task_from_args(&mut brain_args);
    if !brain_args.iter().any(|a| a == "--session") {
        brain_args.push("--session".into());
        brain_args.push(session_id.clone());
    }

    // Spawn brain: stdin=pipe (spine→brain Feedback), stdout=pipe (brain→spine Signals),
    // stderr=inherit (brain UI output goes to terminal).
    let exe = std::env::current_exe().unwrap_or_else(|e| {
        sink.diagnostic(Diag::Error, &format!("error: current_exe: {e}"));
        std::process::exit(1);
    });
    let mut child = Command::new(&exe)
        .arg("__brain")
        .args(&brain_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .env("GAIT_BRAIN_API_KEY", &api_key)
        .spawn()
        .unwrap_or_else(|e| {
            sink.diagnostic(Diag::Error, &format!("error spawning __brain: {e}"));
            std::process::exit(1);
        });

    let child_stdin = Arc::new(Mutex::new(
        child.stdin.take().expect("brain stdin should be piped"),
    ));
    let mut child_stdout = child.stdout.take().expect("brain stdout should be piped");

    // Relay loop: read Signal frames from brain stdout.
    let mut run_result_json = String::new();
    let mut brain_failed = false;
    let sandbox_cfg = config.sandbox.clone();
    let sandbox_on = config.sandbox.on;

    loop {
        let signal = match nerve::read_signal(&mut child_stdout).await {
            Ok(s) => s,
            Err(_) => break, // brain exited / pipe closed
        };
        let corr_id = signal.corr_id;
        match signal.body {
            Some(signal::Body::Impulse(impulse)) => {
                // Spawn a task to run the tool and send Feedback back.
                let stdin_writer = Arc::clone(&child_stdin);
                let cfg_clone = sandbox_cfg.clone();
                let gait_dir_c = gait_dir.clone();
                let codebase_c = codebase_path.clone();
                let session_c = session_id.clone();
                let exe_c = exe.clone();
                tokio::spawn(async move {
                    let feedback = run_tool_for_spine(
                        impulse,
                        &cfg_clone,
                        &gait_dir_c,
                        &codebase_c,
                        &session_c,
                        sandbox_on,
                        &exe_c,
                    )
                    .await;
                    let resp = Signal {
                        corr_id,
                        body: Some(signal::Body::Feedback(feedback)),
                    };
                    let mut w = stdin_writer.lock().await;
                    nerve::write_signal(&mut *w, &resp).await.ok();
                });
            }
            Some(signal::Body::Append(append)) => {
                if let Some(evt) = append.event {
                    gait_proto::wire::append_with_id(&log_path, &evt).ok();
                }
            }
            Some(signal::Body::StreamForward(fwd)) => {
                // Brain sends all streaming items via StreamForward so that any
                // spine (CLI or REPL) can render them. CLI spine forwards to its
                // CliSink; REPL spine (relay_brain_turn) forwards to ReplSink.
                if let Some(item) = fwd.item {
                    sink.item(&item);
                }
            }
            Some(signal::Body::Terminated(t)) => {
                run_result_json = t.run_result_json;
                brain_failed = t.failed;
                break;
            }
            Some(signal::Body::Spawn(sp)) => {
                // Brain delegates process-spawn to spine (brain may be sealed).
                // Append --sandbox-locked so the child cannot disable sandboxing.
                let mut argv = sp.argv;
                if sandbox_on {
                    argv.push("--sandbox-locked".to_string());
                }
                let _ = Command::new(&exe)
                    .args(&argv)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
            _ => {}
        }
    }

    let _ = child.wait().await;

    if !run_result_json.is_empty() {
        println!("{}", run_result_json);
    }
    if brain_failed {
        std::process::exit(1);
    }
}

/// Remove `--task <value>` from a brain argv slice (brain runs in Resume mode).
fn strip_task_from_args(args: &mut Vec<String>) {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--task" {
            args.remove(i);
            if i < args.len() {
                args.remove(i); // remove the value
            }
        } else {
            i += 1;
        }
    }
}

/// Spine-side tool execution: compute scope ceiling, spawn `__run-tool`, return Feedback.
/// Mirrors `SubprocessSynapse::fire` but uses `bound_request` for scope and takes
/// the impulse directly from the Nerve rather than a `Tool` ref.
async fn run_tool_for_spine(
    impulse: gait_proto::gait::sandbox::v1::Impulse,
    sandbox_cfg: &gait_core::config::SandboxConfig,
    gait_dir: &PathBuf,
    codebase: &PathBuf,
    session_id: &str,
    sandbox_on: bool,
    exe: &std::path::Path,
) -> gait_proto::gait::sandbox::v1::Feedback {
    use tokio::io::AsyncWriteExt;

    // Create a per-call temp jail (same pattern as SubprocessSynapse).
    let jail_raw = std::env::temp_dir().join(format!(
        "gait-hand-{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    ));
    if std::fs::create_dir_all(&jail_raw).is_err() {
        return error_feedback("failed to create temp jail");
    }
    let jail = jail_raw.canonicalize().unwrap_or_else(|_| jail_raw.clone());

    // Spine computes authoritative scope ceiling — ignores brain's claimed scope.
    let scope =
        gait_core::sandbox::ceiling::bound_request(&impulse.tool, sandbox_cfg, codebase, &jail);
    let requested = scope_to_proto(&scope);

    let impulse_with_scope = gait_proto::gait::sandbox::v1::Impulse {
        tool: impulse.tool.clone(),
        args: impulse.args,
        cwd: impulse.cwd,
        requested: Some(requested),
    };
    let bytes = impulse_with_scope.encode_to_vec();

    let mut child = match Command::new(exe)
        .arg("__run-tool")
        .arg("--gait-dir")
        .arg(gait_dir)
        .arg("--codebase")
        .arg(codebase)
        .arg("--session")
        .arg(session_id)
        .arg("--tool")
        .arg(&impulse.tool)
        .arg("--seal")
        .arg(if sandbox_on { "true" } else { "false" })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| {
            matches!(
                k.as_str(),
                "PATH" | "HOME" | "USER" | "SHELL" | "TERM" | "LANG" | "LC_ALL" | "TMPDIR"
            )
        }))
        .env("TMPDIR", &jail)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("failed to spawn __run-tool: {e}"));
        }
    };

    {
        let mut stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                let _ = std::fs::remove_dir_all(&jail);
                return error_feedback("no stdin on __run-tool");
            }
        };
        if let Err(e) = stdin.write_all(&bytes).await {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("write impulse failed: {e}"));
        }
    }

    let out = match child.wait_with_output().await {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("wait_with_output failed: {e}"));
        }
    };
    let _ = std::fs::remove_dir_all(&jail);

    if out.status.success() {
        gait_proto::gait::sandbox::v1::Feedback::decode(out.stdout.as_slice())
            .unwrap_or_else(|e| error_feedback(&format!("bad Feedback: {e}")))
    } else {
        error_feedback(&format!(
            "hand exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Build a Feedback carrying a string error result.
fn error_feedback(msg: &str) -> gait_proto::gait::sandbox::v1::Feedback {
    gait_proto::gait::sandbox::v1::Feedback {
        result: Some(pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::StringValue(msg.to_string())),
        }),
        events: vec![],
        denied: None,
    }
}

/// Convert SandboxScope to proto Scope. Duplicated from synapse.rs (private there).
fn scope_to_proto(s: &gait_core::sandbox::SandboxScope) -> Scope {
    use gait_core::sandbox::NetworkPolicy;
    Scope {
        read_all: s.read_all,
        readable: s
            .readable
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        writable: s
            .writable
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        network: Some(match &s.network {
            NetworkPolicy::Deny => NetworkScope {
                mode: Mode::Deny as i32,
                hosts: vec![],
            },
            NetworkPolicy::All => NetworkScope {
                mode: Mode::All as i32,
                hosts: vec![],
            },
            NetworkPolicy::Hosts(h) => NetworkScope {
                mode: Mode::Hosts as i32,
                hosts: h.clone(),
            },
        }),
    }
}

// ── Brain ─────────────────────────────────────────────────────────────────────
// The brain runtime (brain_scope, BrainSink, BrainDriver, run_brain) has been
// moved to gait-core/src/turn/brain.rs. The CLI wrapper (`cmd_brain`) lives in
// args.rs and calls `gait_core::turn::brain::run_brain`.
