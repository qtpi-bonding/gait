//! Spine-side helpers shared by the CLI spine (`gait run`) and the REPL spine
//! (`gait chat`).  Both spawn a `gait __brain` per turn and relay its Nerve
//! signals; this module owns that shared relay loop and the tool-execution helper.

use std::path::Path;
use std::sync::Arc;

use prost::Message as _;
use tokio::sync::Mutex;

use crate::config::SandboxConfig;
use crate::port::output::TurnSink;
use crate::proto::gait::sandbox::v1::{
    network_scope::Mode, signal, Feedback, Impulse, NetworkScope, Notify, Scope, Signal,
};
#[allow(unused_imports)] // used in test-only wire::append calls
use crate::proto::wire;
use crate::sandbox::{NetworkPolicy, SandboxScope};
use crate::turn::nerve;
use crate::turn::DiagnosticSink;

// ── WatchdogCfg + watchdog_breach ────────────────────────────────────────────

use crate::diagnostic::{Cap, Phase};

#[derive(Clone, Copy)]
pub(crate) struct WatchdogCfg {
    pub stall_cap_ms: u64,
    pub ping_ms: u64,
    pub absolute_backstop_ms: u64, // guard_idle absolute_cap(600s) + 60s margin
}

impl Default for WatchdogCfg {
    fn default() -> Self {
        // `GAIT_TEST_WATCHDOG_MS` / `GAIT_TEST_WATCHDOG_PING_MS` shrink the caps
        // for integration tests so the watchdog fires in seconds. Never set in production.
        let stall_cap_ms = std::env::var("GAIT_TEST_WATCHDOG_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90_000);
        let ping_ms = std::env::var("GAIT_TEST_WATCHDOG_PING_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15_000);
        Self {
            stall_cap_ms,
            ping_ms,
            absolute_backstop_ms: 660_000,
        }
    }
}

/// Pure decision. `since_last_ping_ms = None` until the first ping (cold-start grace:
/// both caps disarmed). `tool_in_flight = !pending_calls.is_empty()` from the spine loop.
pub(crate) fn watchdog_breach(
    cfg: &WatchdogCfg,
    phase: Phase,
    phase_elapsed_ms: u64,
    since_last_ping_ms: Option<u64>,
    tool_in_flight: bool,
) -> Option<&'static str> {
    let since = since_last_ping_ms?;
    if since > 2 * cfg.ping_ms {
        return Some("no diagnostic ping (brain wedged)");
    }
    match phase.cap() {
        Cap::StallCap => {
            (phase_elapsed_ms > cfg.stall_cap_ms).then_some("phase exceeded stall_cap")
        }
        Cap::AbsoluteBackstop => (phase_elapsed_ms > cfg.absolute_backstop_ms)
            .then_some("streaming exceeded absolute backstop"),
        Cap::ToolAware => {
            if tool_in_flight {
                None
            } else {
                (phase_elapsed_ms > cfg.stall_cap_ms)
                    .then_some("await_feedback stalled, no tool in flight")
            }
        }
    }
}

// ── ProfileInputs + build_turn_profile ───────────────────────────────────────

/// Inputs the spine needs to build the turn's `Profile` (parsed by the CLI/REPL
/// caller, since gait-core cannot parse clap args). Mirrors the brain's old
/// agent/model/allow/deny handling.
#[derive(Clone)]
pub struct ProfileInputs {
    pub agent_name: Option<String>,
    pub model_override: Option<String>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub result_schema: Option<serde_json::Value>,
}

/// Build the turn's `Profile` on the spine (disk scan: agent file, system-prompt
/// layers, skills/mcp/plugins). Folds CLI allow/deny into config first, mirroring
/// the brain's previous in-process logic.
pub fn build_turn_profile(
    gait_dir: &std::path::Path,
    codebase: &std::path::Path,
    inputs: &ProfileInputs,
) -> crate::profile::Profile {
    use crate::config::resolver::{ConfigResolver, Overrides};
    let loaded = crate::config::load_config(gait_dir);
    // consumer layer: nothing (gait.jsonc is already baked into `loaded`)
    // per-call layer: CLI allow/deny overrides (only when non-empty, to preserve additive semantics)
    let per_call = Overrides {
        allow: if inputs.allow.is_empty() {
            None
        } else {
            // merge: start from loaded allow and add any new patterns
            let mut merged = loaded.allow.clone();
            for p in &inputs.allow {
                if !merged.contains(p) {
                    merged.push(p.clone());
                }
            }
            Some(merged)
        },
        deny: if inputs.deny.is_empty() {
            None
        } else {
            let mut merged = loaded.deny.clone();
            for p in &inputs.deny {
                if !merged.contains(p) {
                    merged.push(p.clone());
                }
            }
            Some(merged)
        },
        ..Overrides::empty()
    };
    let mut cfg = ConfigResolver::new(loaded)
        .with(Overrides::empty())
        .with(per_call)
        .resolve();
    if let Some(s) = &inputs.result_schema {
        cfg.result_schema = Some(s.clone());
    }
    let agent_path: Option<std::path::PathBuf> = match &inputs.agent_name {
        Some(name) => Some(crate::agent_frontmatter::resolve_markdown_context_path(
            gait_dir,
            crate::agent_frontmatter::MarkdownKind::Agent,
            name,
        )),
        None => {
            let default = crate::agent_frontmatter::resolve_markdown_context_path(
                gait_dir,
                crate::agent_frontmatter::MarkdownKind::Agent,
                "default",
            );
            default.exists().then_some(default)
        }
    };
    crate::profile::Profile::from_config_and_agent(
        &cfg,
        gait_dir,
        Some(codebase),
        agent_path.as_deref(),
        inputs.model_override.clone(),
    )
}

// ── brain_argv ────────────────────────────────────────────────────────────────

/// Build the argv passed to `gait __brain` — the single source of truth for the
/// brain's command line. The `"__brain"` role token itself is prepended by the
/// spawn in `relay_brain_turn`; no consumer constructs any of this.
///
/// Only flags the brain actually reads from argv are emitted: `--gait-dir`,
/// `--codebase`, `--session`, `--auto`, `--max-iter`, `--allow`, `--deny`, and
/// the sandbox flag. The profile (model/agent/result-schema) reaches the brain
/// via the `BrainInit` frame, not argv.
pub(crate) fn brain_argv(
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    auto: bool,
    max_iter: Option<usize>,
    sandbox_on: bool,
    profile: &ProfileInputs,
) -> Vec<String> {
    let mut a = vec![
        "--gait-dir".to_string(),
        gait_dir.to_string_lossy().into_owned(),
        "--codebase".to_string(),
        codebase.to_string_lossy().into_owned(),
        "--session".to_string(),
        session_id.to_string(),
    ];
    if auto {
        a.push("--auto".to_string());
    }
    if let Some(n) = max_iter {
        a.push("--max-iter".to_string());
        a.push(n.to_string());
    }
    for p in &profile.allow {
        a.push("--allow".to_string());
        a.push(p.clone());
    }
    for p in &profile.deny {
        a.push("--deny".to_string());
        a.push(p.clone());
    }
    // Sealed spine ⇒ sealed brain (and vice versa); the brain never escapes a
    // sandboxed parent and is never sandboxed under an unsandboxed parent.
    a.push(
        if sandbox_on {
            "--sandbox-locked"
        } else {
            "--no-sandbox"
        }
        .to_string(),
    );
    a
}

// ── relay_brain_turn ─────────────────────────────────────────────────────────

/// Shared slot holding the active cascade group for the current turn.
/// The signal handler (SIGINT/SIGTERM) reads this to call `kill_all()`.
pub type SignalSlot =
    std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::process::Containment>>>>;

/// Spawn `gait __brain` for one turn and relay its Nerve signals.
///
/// * `Impulse`       → `run_tool_as_spine` (spawned task) → `Feedback`
/// * `Append`        → `wire::append_with_id` (spine is sole log writer)
/// * `StreamForward` → `sink.item()` (REPL TUI rendering)
/// * `Terminated`    → parse `TurnResult`, `sink.end_turn()`, `sink.result()`, return
///
/// The spine is the sole gait-dir writer. Before spawning the brain, it performs
/// all startup disk writes (worktree, checkpoint, session.json, lock, SessionStart)
/// so the brain can run with `writable: []`. The brain's `run_turn` skips these
/// when `relay.is_some()`.
///
/// Returns `Ok(true)` if the brain signalled failure, `Ok(false)` on success.
#[allow(clippy::too_many_arguments)]
pub async fn relay_brain_turn(
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    api_key: &str,
    exe: &Path,
    sandbox_cfg: &SandboxConfig,
    sandbox_on: bool,
    auto: bool,
    max_iter: Option<usize>,
    profile_inputs: &ProfileInputs,
    sink: &mut dyn TurnSink,
    cascade_group_arg: Option<String>,
    // Optional shared slot: if provided, the established group is stored here
    // for the duration of the turn so an external signal handler can call
    // kill_all(). The caller must clear the slot after the turn completes.
    signal_slot: Option<&SignalSlot>,
    repos: &crate::git::Repos,
) -> anyhow::Result<bool> {
    // ── Establish (or inherit) the cascade group for this turn ─────────────
    let group = crate::process::establish(cascade_group_arg);

    // Store the group in the signal slot (if provided) so SIGINT/SIGTERM can
    // reap it. Sequenced: establish → store → run → kill_all → caller clears.
    if let Some(slot) = signal_slot {
        *slot.lock().unwrap() = Some(Arc::clone(&group));
    }
    // ── Build the turn context once ────────────────────────────────────────
    // The ctx carries every spine-side handle the turn needs (gait-dir,
    // codebase, session id, repos, exe, cascade group, sandbox flag). Building
    // it here means the persistence types can later be constructed from `&ctx`
    // and the relay loop can clone it cheaply into spawned tasks.
    let ctx = TurnCtx {
        gait_dir: gait_dir.to_path_buf(),
        codebase: codebase.to_path_buf(),
        session_id: session_id.to_string(),
        repos: repos.clone(),
        exe: exe.to_path_buf(),
        group: Arc::clone(&group),
        sandbox_on,
    };

    // Sole owner of diagnostic.jsonl writes for this turn. Built once after
    // TurnCtx so the three call sites can use `&diag` without rebuilding the
    // path. See src/turn/diag_sink.rs for the gating/terminal discipline.
    let diag = DiagnosticSink::new(gait_dir, session_id);

    // ── Spine-owned startup writes (brain is disk-write-free) ───────────────
    // These all write to gait-dir; the brain's seal has writable: [] so it
    // cannot do them. The spine does them before spawning the brain.
    // `repos` is injected by the caller (DI) — production callers pass
    // `Repos::production()`; test harnesses pass a recording mock.
    let session_dir = gait_dir.join("sessions").join(session_id);
    let tx_path = session_dir.join("log.jsonl");

    // 1. Session worktree (idempotent — callers may have already done this).
    repos.traces.create_session_worktree(gait_dir, session_id)?;

    // 2. Checkpoint repo init (idempotent — checks if HEAD exists).
    repos.checkpoints.init(gait_dir, session_id, codebase)?;

    // 3. Session lock — held for the duration of the brain process.
    let log = crate::turn::SessionLog::open(&session_dir)?;

    // 5. SessionStart event — write once per session (idempotent via event check).
    {
        use crate::proto::gait::log::v1::{event, payload};
        let ingestor = crate::session::Ingestor::from_session(gait_dir, session_id);
        let has_start = ingestor.events().unwrap_or_default().iter().any(|e| {
            matches!(
                e.body.as_ref().and_then(|b| match b {
                    event::Body::Payload(p) => p.kind.as_ref(),
                    _ => None,
                }),
                Some(payload::Kind::SessionStart(_))
            )
        });
        if !has_start {
            let codebase_hash = repos.codebase.head_hash(codebase).unwrap_or_default();
            let dirty_files = repos.codebase.dirty_files(codebase);
            let codebase_dirty = !dirty_files.is_empty();
            crate::session::log::init_session(crate::session::log::InitSessionOpts {
                log_path: tx_path.clone(),
                id: session_id.to_string(),
                model: String::new(),
                codebase_hash,
                codebase_dirty,
                assigned_by: None,
                codebase_path: Some(codebase.to_string_lossy().into_owned()),
                notify_done: false,
            })?;
        }
    }

    // ── Spawn brain (enrolled as cascade group leader) ──────────────────────
    let mut child = {
        use tokio::process::Command as TokioCommand;
        let brain_args = brain_argv(
            gait_dir,
            codebase,
            session_id,
            auto,
            max_iter,
            sandbox_on,
            profile_inputs,
        );
        let mut std_cmd = std::process::Command::new(exe);
        std_cmd
            .arg("__brain")
            .args(&brain_args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .env("GAIT_BRAIN_API_KEY", api_key);
        // Set up cascade group enrollment hooks (setsid for brain = first call).
        group
            .configure_enrolled(&mut std_cmd)
            .map_err(|e| anyhow::anyhow!("failed to configure brain enrollment: {e}"))?;
        // Convert to tokio Command (keeps all pre_exec hooks) and spawn async.
        let mut tokio_cmd = TokioCommand::from(std_cmd);
        let child = tokio_cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn __brain: {e}"))?;
        // Register brain pid so group can reap it and knows its pgid.
        group.register_pid(child.id().unwrap_or(0) as i32);
        child
    };
    // Capture the group handle now that the brain (leader) is realized.
    let _group_handle = group.handle(); // Future: pass to blocking subagents (Plan 4).

    let child_stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
    let mut child_stdout = child.stdout.take().expect("piped stdout");

    let log_path = log.log_path().to_path_buf();

    // ── BrainInit: push config + full log + git so the brain reads zero disk. ──
    // The caller appended this turn's user input before relay_brain_turn, and the
    // SessionStart write above ran, so `events` is complete.
    {
        use crate::proto::gait::sandbox::v1::{signal, BrainInit, DaemonStatus, GitStatus, Signal};
        use tokio::io::AsyncWriteExt;
        let cfg = crate::config::load_config(gait_dir);
        let config_json = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());
        let events = crate::session::log::read_events(&log_path).unwrap_or_default();
        let head_hash = repos.codebase.head_hash(codebase).unwrap_or_default();
        let dirty_files = repos.codebase.dirty_files(codebase);
        let profile = build_turn_profile(gait_dir, codebase, profile_inputs);
        let profile_json = serde_json::to_string(&profile).unwrap_or_else(|_| "null".to_string());
        let daemons = {
            use crate::session::daemons::{active, is_alive};
            active(gait_dir)
                .unwrap_or_default()
                .into_iter()
                .map(|r| DaemonStatus {
                    alive: is_alive(&r),
                    name: r.name,
                    pid: r.pid,
                    log_path: r.log_path,
                })
                .collect::<Vec<_>>()
        };
        let init = BrainInit {
            config_json,
            events,
            git: Some(GitStatus {
                head_hash,
                dirty_files,
            }),
            profile_json,
            daemons,
        };
        let sig = Signal {
            corr_id: 0,
            body: Some(signal::Body::BrainInit(init)),
        };
        let mut w = child_stdin.lock().await;
        nerve::write_signal(&mut *w, &sig).await?;
        w.flush().await?;
    }

    let mut run_result: Option<crate::turn::TurnResult> = None;
    let mut brain_failed = false;
    // Track in-flight ToolCall call_ids so we can synthesize a ToolResult for
    // any that are orphaned if the brain is killed before it returns results.
    let mut pending_calls: std::collections::HashSet<String> = std::collections::HashSet::new();

    let wd = WatchdogCfg::default();
    let mut last_phase = crate::diagnostic::Phase::TurnBegin;
    let mut last_phase_elapsed_ms: u64 = 0;
    // None until the first ping → cold-start grace (watchdog disarmed).
    let mut last_ping_at: Option<std::time::Instant> = None;

    sink.begin_turn();
    loop {
        let read = nerve::read_signal(&mut child_stdout);
        let timed = tokio::time::timeout(std::time::Duration::from_millis(wd.ping_ms), read).await;
        let signal = match timed {
            Ok(Ok(s)) => s,
            Ok(Err(_)) => break, // pipe closed (brain exited)
            Err(_) => {
                // read timed out → fall through to the watchdog check
                let since = last_ping_at.map(|t| t.elapsed().as_millis() as u64);
                if let Some(reason) = watchdog_breach(
                    &wd,
                    last_phase,
                    last_phase_elapsed_ms,
                    since,
                    !pending_calls.is_empty(),
                ) {
                    emit_terminal_failure(&log, &diag, &mut brain_failed, last_phase, reason);
                    let _ = group.kill_all();
                    break;
                }
                continue;
            }
        };
        let corr_id = signal.corr_id;
        match signal.body {
            Some(signal::Body::Impulse(impulse)) => {
                let stdin_writer = Arc::clone(&child_stdin);
                if impulse.tool == "spawn_agent" || impulse.tool == "run_workflow" {
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        let feedback = if impulse.tool == "spawn_agent" {
                            let args = parse_spawn_args(&impulse);
                            handle_spawn_agent(args, ctx).await
                        } else {
                            let args = parse_run_workflow_args(&impulse);
                            handle_run_workflow(args, ctx).await
                        };
                        reply_feedback(stdin_writer, corr_id, feedback).await;
                    });
                } else {
                    let cfg = sandbox_cfg.clone();
                    let gd = gait_dir.to_path_buf();
                    let cb = codebase.to_path_buf();
                    let sid = session_id.to_string();
                    let exe_c = exe.to_path_buf();
                    let grp = Arc::clone(&group);
                    tokio::spawn(async move {
                        let feedback = run_tool_as_spine(
                            impulse, &cfg, &gd, &cb, &sid, sandbox_on, &exe_c, grp,
                        )
                        .await;
                        reply_feedback(stdin_writer, corr_id, feedback).await;
                    });
                }
            }
            Some(signal::Body::Append(append)) => {
                if let Some(mut evt) = append.event {
                    // Track in-flight ToolCall / ToolResult pairs by call_id.
                    // On cancel, any remaining pending_calls are orphans that need
                    // a synthesized ToolResult so retrace reconstructs cleanly.
                    use crate::proto::gait::log::v1::{event, payload};
                    if let Some(event::Body::Payload(ref p)) = evt.body {
                        match p.kind.as_ref() {
                            Some(payload::Kind::ToolCall(tc)) => {
                                pending_calls.insert(tc.call_id.clone());
                            }
                            Some(payload::Kind::ToolResult(tr)) => {
                                pending_calls.remove(&tr.call_id);
                            }
                            _ => {}
                        }
                    }
                    enrich_permissions_preview(
                        &mut evt,
                        codebase,
                        gait_dir,
                        session_id,
                        sandbox_cfg,
                    );
                    log.relay_event(&evt).ok();
                }
            }
            Some(signal::Body::StreamForward(fwd)) => {
                if let Some(mut item) = fwd.item {
                    if let Some(crate::proto::gait::log::v1::stream_item::Item::Event(
                        ref mut evt,
                    )) = item.item
                    {
                        enrich_permissions_preview(
                            evt,
                            codebase,
                            gait_dir,
                            session_id,
                            sandbox_cfg,
                        );
                    }
                    sink.item(&item);
                }
            }
            Some(signal::Body::Terminated(t)) => {
                brain_failed = t.failed;
                // Brain-reported failure: the brain already appended its own Error
                // event to log.jsonl via Signal::Append before sending Terminated,
                // so we must NOT double-write the event here. We do write a
                // diagnostic record so the failure is always explained in
                // diagnostic.jsonl regardless of GAIT_DEBUG.
                if t.failed {
                    let rec = crate::diagnostic::DiagRecord::terminal(
                        session_id,
                        last_phase,
                        "brain reported failure via Terminated signal",
                    );
                    diag.terminal(&rec.to_json_line());
                }
                run_result = Some(turn_result_from_json(&t.run_result_json));
                break;
            }
            Some(signal::Body::Spawn(sp)) => {
                // Brain (sealed) delegates process-launch to spine.
                // Append --sandbox-locked so the child cannot disable sandboxing.
                let mut argv = sp.argv;
                if sandbox_on {
                    argv.push("--sandbox-locked".to_string());
                }
                if sp.detached {
                    // Peer agent: breaks out of the cascade group — survives parent death.
                    let mut cmd = std::process::Command::new(exe);
                    cmd.args(&argv)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null());
                    let _ = group.spawn_detached(cmd);
                } else {
                    // Subagent: enrolled in parent's cascade group — reaped when parent ends.
                    // Pass --cascade-group so the child self-assigns (race-free enrollment).
                    if let Some(handle) = group.handle() {
                        argv.extend(["--cascade-group".to_string(), handle]);
                    } else {
                        eprintln!("[spine] warning: spawn_agent cascade-group handle unavailable; subagent will not be enrolled in cascade group");
                    }
                    let mut cmd = std::process::Command::new(exe);
                    cmd.args(&argv)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null());
                    let _ = group.spawn_enrolled(cmd);
                }
            }
            Some(signal::Body::Notify(Notify {
                target_session,
                from_session,
                status,
                summary,
            })) => {
                // Spine is sole gait-dir writer: write agent_update to parent session.
                let event = crate::proto::events::agent_update(
                    from_session.clone(),
                    &status,
                    summary.clone(),
                    None,
                );
                let target_dir = gait_dir.join("sessions").join(&target_session);
                match crate::turn::SessionLog::open_retrying(&target_dir, 20).await {
                    Ok(target_log) => {
                        target_log.append_body(event).ok();
                        // Use the injected `repos` (DI) — same handle as every other
                        // persistence site in this turn, so a test mock observes this
                        // cross-session commit too. (Was `Repos::production()`.)
                        repos
                            .traces
                            .commit_session(
                                gait_dir,
                                &target_session,
                                &format!(
                                    "session({}): agent_update from {}",
                                    target_session, from_session
                                ),
                            )
                            .ok();
                    }
                    Err(e) => {
                        eprintln!("[spine] warning: failed to acquire lock for notify: {e}");
                    }
                }
                // No Feedback needed — one-way signal like Append.
            }
            Some(signal::Body::Diagnostic(diag_msg)) => {
                last_phase = crate::diagnostic::phase_from_str(&diag_msg.phase);
                last_phase_elapsed_ms = diag_msg.elapsed_ms;
                last_ping_at = Some(std::time::Instant::now());
                diag.ping(&diag_msg.record_json);
            }
            _ => {}
        }

        // Watchdog: runs after every signal (in addition to every read-timeout).
        let since = last_ping_at.map(|t| t.elapsed().as_millis() as u64);
        if let Some(reason) = watchdog_breach(
            &wd,
            last_phase,
            last_phase_elapsed_ms,
            since,
            !pending_calls.is_empty(),
        ) {
            emit_terminal_failure(&log, &diag, &mut brain_failed, last_phase, reason);
            let _ = group.kill_all();
            break;
        }
    }
    sink.end_turn();

    // Synthesize a ToolResult for each orphaned ToolCall (brain was killed before
    // it could emit results). This ensures retrace::messages reconstructs with no
    // orphan tool calls. Must run before commit_session so the repair lands in the
    // same commit as the rest of the transcript.
    for call_id in pending_calls.drain() {
        let body =
            crate::proto::events::tool_result(call_id, "cancelled by user".into(), None, None);
        let prev = crate::session::log::last_event(&log_path).ok().flatten();
        let ev = crate::proto::envelope::mint(body, prev.as_ref(), 0, 0);
        log.relay_event(&ev).ok();
    }

    // Drop the brain's stdin writer to signal EOF — the brain's Nerve reader
    // task is blocking on read_signal from stdin and will exit on EOF, which
    // lets the brain's tokio runtime shut down and the process exit.
    drop(child_stdin);

    let _ = child.wait().await;

    // Reap any stragglers (hands, sh grandchildren) the brain left behind.
    // Idempotent — no-op if nothing remains in the group.
    let _ = group.kill_all();

    if let Some(result) = &run_result {
        // The spine is the sole gait-dir writer. The brain skipped
        // commit_session and checkpoint_codebase (writable: []) — the spine
        // does both here, after the brain exits, so the session worktree and
        // shadow checkpoint repo reflect the transcript at this exitable
        // boundary.
        let msg = match result {
            crate::turn::TurnResult::Done { .. } => {
                format!("session({}): turn complete", session_id)
            }
            crate::turn::TurnResult::Cancelled => {
                format!("session({}): cancelled", session_id)
            }
            crate::turn::TurnResult::Permissions => {
                format!("session({}): permissions", session_id)
            }
            crate::turn::TurnResult::Clarify { .. } => {
                format!("session({}): clarify", session_id)
            }
            crate::turn::TurnResult::Inform { .. } => {
                format!("session({}): inform", session_id)
            }
            crate::turn::TurnResult::Unlock { .. } => {
                format!("session({}): unlock", session_id)
            }
        };

        // Checkpoint the codebase (shadow repo in gait-dir). Best-effort — a
        // failure means `gait rewind` won't have a snapshot for this turn,
        // but the turn itself is complete.
        let events = crate::session::log::read_events(&log_path).unwrap_or_default();
        let turn_n = crate::session::log::count_turn_complete(&events) as u32;
        let _ = repos
            .checkpoints
            .checkpoint(gait_dir, session_id, codebase, turn_n);

        // Commit the transcript to the session worktree.
        let _ = repos.traces.commit_session(gait_dir, session_id, &msg);
    }

    if let Some(result) = run_result {
        sink.result(&result);
    }

    Ok(brain_failed)
}

// ── run_tool_as_spine ────────────────────────────────────────────────────────

/// Spawn `gait __run-tool` for one tool invocation (spine's authoritative scope
/// ceiling applied via `bound_request`). Mirrors `SubprocessSynapse::fire` but
/// takes an `Impulse` directly from the Nerve rather than a `Tool` ref.
#[allow(clippy::too_many_arguments)]
pub async fn run_tool_as_spine(
    impulse: Impulse,
    sandbox_cfg: &SandboxConfig,
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    sandbox_on: bool,
    exe: &Path,
    group: std::sync::Arc<dyn crate::process::Containment>,
) -> Feedback {
    use tokio::io::AsyncWriteExt;

    let jail_raw = std::env::temp_dir().join(format!(
        "gait-hand-{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    ));
    if std::fs::create_dir_all(&jail_raw).is_err() {
        return error_feedback("failed to create temp jail");
    }
    let jail = jail_raw.canonicalize().unwrap_or_else(|_| jail_raw.clone());

    // For detached daemon shell calls, use the full agent ceiling (all capabilities
    // enabled, bounded only by the session sandbox config) rather than the
    // per-call tool scope. This lets the daemon process inherit the agent's
    // configured limits (e.g. network, codebase writes) rather than the narrower
    // default shell scope.
    let scope = if impulse.tool == "shell"
        && impulse
            .args
            .as_ref()
            .and_then(|s| s.fields.get("detached"))
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::BoolValue(b)) => Some(*b),
                _ => None,
            })
            .unwrap_or(false)
    {
        use crate::types::ToolAccess;
        crate::sandbox::ceiling::compute_scope(
            ToolAccess {
                reads: true,
                writes: true,
                network: true,
                spawns: true,
            },
            sandbox_cfg,
            codebase,
            &jail,
        )
    } else {
        crate::sandbox::ceiling::bound_request(&impulse.tool, sandbox_cfg, codebase, &jail)
    };
    let requested = scope_to_proto(&scope);

    // Resolve cwd: if the brain sent a cwd in the Impulse, use it; otherwise
    // read active_cwd from the session log (same as SubprocessSynapse did).
    // RelaySynapse sends empty cwd because the brain doesn't read the session
    // log for cwd — the spine owns it.
    let cwd = if impulse.cwd.is_empty() {
        let events = crate::session::log::read_events(
            &gait_dir.join("sessions").join(session_id).join("log.jsonl"),
        )
        .unwrap_or_default();
        crate::session::retrace::active_cwd(&events, codebase)
            .to_string_lossy()
            .into_owned()
    } else {
        impulse.cwd.clone()
    };

    let impulse_with_scope = Impulse {
        tool: impulse.tool.clone(),
        args: impulse.args,
        cwd,
        requested: Some(requested),
    };
    let bytes = impulse_with_scope.encode_to_vec();

    // debug_assert: the brain (group leader) must be alive before we enroll a hand.
    // pgid == 0 means spawn_enrolled hasn't been called yet for the brain — logic error.
    #[cfg(debug_assertions)]
    {
        let handle_str = group.handle();
        debug_assert!(
            handle_str.is_some(),
            "hand enrolled before brain: cascade group has no leader yet"
        );
    }
    let mut child = {
        use tokio::process::Command as TokioCommand;
        let mut std_cmd = std::process::Command::new(exe);
        std_cmd
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
            .env("TMPDIR", &jail);
        // Enroll hand into the cascade group (joins brain's process group).
        if let Err(e) = group.configure_enrolled(&mut std_cmd) {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("failed to configure hand enrollment: {e}"));
        }
        let mut tokio_cmd = TokioCommand::from(std_cmd);
        match tokio_cmd.spawn() {
            Ok(c) => {
                group.register_pid(c.id().unwrap_or(0) as i32);
                c
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&jail);
                return error_feedback(&format!("failed to spawn __run-tool: {e}"));
            }
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

    // The hand always writes a HandResponse on stdout. Decode it regardless of
    // exit code; only fall back to exit-code/stderr if stdout is missing/garbled
    // (a true crash — the hand never got to write).
    use gait_proto::gait::sandbox::v1::{hand_response, HandResponse};
    let feedback = match HandResponse::decode(out.stdout.as_slice()) {
        Ok(HandResponse {
            outcome: Some(hand_response::Outcome::Feedback(f)),
        }) => f,
        Ok(HandResponse {
            outcome: Some(hand_response::Outcome::Error(e)),
        }) => error_feedback(&format!("hand error: {}", e.message)),
        Ok(HandResponse { outcome: None }) => error_feedback("hand returned empty HandResponse"),
        Err(_) => error_feedback(&format!(
            "hand crashed (exit {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )),
    };

    // If the hand spawned a detached daemon, record it in daemons.jsonl.
    // The spine is the sole gait-dir writer; best-effort (ignore errors).
    if let Some(ref rec) = feedback.daemon {
        let dr = crate::session::daemons::DaemonRecord {
            name: rec.name.clone(),
            pid: rec.pid,
            pgid: rec.pgid,
            started_at: rec.started_at,
            cmd: rec.cmd.clone(),
            cwd: rec.cwd.clone(),
            log_path: rec.log_path.clone(),
        };
        let _ = crate::session::daemons::append_start(gait_dir, &dr);
    }

    feedback
}

// ── Helpers ──────────────────────────────────────────────────────────────────
/// THE single dual-emit site for terminal failures. Ordering is load-bearing:
/// the Error event lands in log.jsonl FIRST (transcript correctness + exit
/// parity via `brain_failed`), the DiagRecord in diagnostic.jsonl SECOND
/// (best-effort). Single-threaded sole writer ⇒ back-to-back.
/// Mirrors the orphan-tool repair append at spine.rs:631-636.
fn emit_terminal_failure(
    log: &crate::turn::SessionLog,
    diag: &crate::turn::DiagnosticSink,
    brain_failed: &mut bool,
    phase: crate::diagnostic::Phase,
    detail: &str,
) {
    // 1) Error event → log.jsonl FIRST (also sets brain_failed for exit parity).
    let body = crate::proto::events::error(detail.to_string(), crate::diagnostic::phase_str(phase));
    let prev = log.last_event();
    let evt = crate::proto::envelope::mint(body, prev.as_ref(), 0, 0);
    log.relay_event(&evt).ok();
    *brain_failed = true;

    // 2) DiagRecord → diagnostic.jsonl SECOND (always, even when GAIT_DEBUG is
    //    off — a terminal failure must always be explained).
    let rec = crate::diagnostic::DiagRecord::terminal(diag.session_id(), phase, detail);
    diag.terminal(&rec.to_json_line());
}

/// Build a Feedback carrying a string error result.
pub fn error_feedback(msg: &str) -> Feedback {
    Feedback {
        result: Some(pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::StringValue(msg.to_string())),
        }),
        events: vec![],
        denied: None,
        daemon: None,
    }
}

/// Convert `SandboxScope` to the proto `Scope` wire type.
pub fn scope_to_proto(s: &SandboxScope) -> Scope {
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

/// Remove `--task <value>` from an arg slice (brain runs Resume mode).
pub fn strip_task_from_args(args: &mut Vec<String>) {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--task" {
            args.remove(i);
            if i < args.len() {
                args.remove(i);
            }
        } else {
            i += 1;
        }
    }
}

/// Reconstruct a `TurnResult` from the JSON that `BrainSink` encodes in
/// `Terminated.run_result_json` (a serialized `RunResult` proto).
///
/// Only `Cancelled` and `Unlock` are faithfully reconstructed — those are the
/// only variants the REPL sink cares about. Other variants produce a minimal
/// `Done`.
fn turn_result_from_json(json: &str) -> crate::turn::TurnResult {
    use crate::proto::gait::log::v1::{run_result, RunResult};
    let rr: RunResult = serde_json::from_str(json).unwrap_or_default();
    match rr.result {
        Some(run_result::Result::Cancelled(_)) => crate::turn::TurnResult::Cancelled,
        Some(run_result::Result::Unlock(u)) => crate::turn::TurnResult::Unlock { items: u.items },
        Some(run_result::Result::Done(d)) => crate::turn::TurnResult::Done {
            message: d.message,
            result: None,
            changes: d.changes,
        },
        _ => crate::turn::TurnResult::Done {
            message: String::new(),
            result: None,
            changes: None,
        },
    }
}

// ── enrich_permissions_preview ───────────────────────────────────────────────

/// Spine-side preview: for a `Permissions` event, fill any empty `preview` on
/// edit/write items by running that tool's own `preview()` in-process. The spine
/// is unsealed and already opens the codebase, so this is where the diff read
/// belongs (the brain stays read-free). Scope-bounded: a target outside the
/// tool's read ceiling yields no preview rather than reading an arbitrary file.
fn enrich_permissions_preview(
    event: &mut crate::proto::gait::log::v1::Event,
    codebase: &Path,
    gait_dir: &Path,
    session_id: &str,
    sandbox_cfg: &SandboxConfig,
) {
    use crate::proto::gait::log::v1::{event, payload};

    let Some(event::Body::Payload(p)) = event.body.as_mut() else {
        return;
    };
    let Some(payload::Kind::Permissions(perm)) = p.kind.as_mut() else {
        return;
    };

    let ctx = crate::tools::ToolCtx::new(
        codebase.to_path_buf(),
        gait_dir.to_path_buf(),
        session_id.to_string(),
    );
    let tools =
        crate::tools::builtin_tools(ctx, vec![], vec![], vec![], crate::git::Repos::production());

    // compute_scope uses the jail only for path arithmetic (pushes it into
    // readable/writable vecs); it never calls canonicalize or metadata on it.
    // A stable sentinel is therefore sufficient — no temp directory needed.
    let jail = std::env::temp_dir().join("gait-preview-jail");

    for item in perm.items.iter_mut() {
        if item.preview.is_some() {
            continue; // already filled (e.g. non-relay path)
        }
        let Some(tool) = tools.iter().find(|t| t.name() == item.tool_name) else {
            continue;
        };
        // Convert tool_args (proto Struct) back to serde_json::Value for the tool API.
        let args: serde_json::Value = item
            .tool_args
            .as_ref()
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or(serde_json::Value::Null);

        // Scope-bound the read: only preview targets within the tool's read ceiling.
        let ceiling =
            crate::sandbox::ceiling::compute_scope(tool.access(), sandbox_cfg, codebase, &jail);
        let targets = tool.scope_targets(&args);
        // Empty targets → no declared scope; treat as out-of-scope (skip preview).
        let in_scope = !targets.is_empty()
            && targets.iter().all(|(path, _dim)| {
                // Canonicalize the target path so /var/... and /private/var/... compare equal.
                let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                ceiling.read_all
                    || ceiling.readable.iter().any(|r| canonical.starts_with(r))
                    || ceiling.writable.iter().any(|w| canonical.starts_with(w))
            });
        if !in_scope {
            continue;
        }

        if let Some(diff) = tool.preview(&args) {
            item.preview = Some(diff);
        }
    }
}

// ── spawn_agent spine handler ─────────────────────────────────────────────────

/// Arguments for `spawn_agent`, deserialized from the Impulse Struct.
pub(super) struct SpawnAgentArgs {
    pub task: String,
    /// Per-spawn result schema written to `sessions/<id>/result_schema.json`
    /// so the child CLI can enforce it via `ProfileInputs.result_schema`.
    pub schema: Option<serde_json::Value>,
    /// Future: prepend files as context (reserved).
    #[allow(dead_code)]
    pub context_files: Option<Vec<String>>,
    pub detached: bool,
}
/// Context the spine carries for the entire turn (assembled once near the top
/// of `relay_brain_turn`, after the cascade group is established, and cloned
/// into spawned tasks as needed).
#[derive(Clone)]
pub(super) struct TurnCtx {
    pub gait_dir: std::path::PathBuf,
    pub codebase: std::path::PathBuf,
    pub session_id: String,
    pub repos: crate::git::Repos,
    pub exe: std::path::PathBuf,
    pub group: std::sync::Arc<dyn crate::process::Containment>,
    pub sandbox_on: bool,
}

/// Build a `Feedback` carrying a JSON value result.
pub(super) fn feedback_result(value: serde_json::Value) -> Feedback {
    // Convert serde_json::Value → pbjson_types::Value via serde_json round-trip.
    // This correctly handles objects/arrays as proto Struct/ListValue rather than
    // stringifying them, so the brain receives a structured proto Value.
    let proto_value: pbjson_types::Value =
        serde_json::from_value(value).unwrap_or(pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::NullValue(0)),
        });
    Feedback {
        result: Some(proto_value),
        events: vec![],
        denied: None,
        daemon: None,
    }
}

/// Parse a proto `Struct` from an Impulse into `SpawnAgentArgs`.
fn parse_spawn_args(impulse: &Impulse) -> SpawnAgentArgs {
    let s = impulse.args.as_ref();
    let get_str = |key: &str| -> Option<String> {
        s.and_then(|st| st.fields.get(key))
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.clone()),
                _ => None,
            })
    };
    let get_bool = |key: &str| -> bool {
        s.and_then(|st| st.fields.get(key))
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::BoolValue(b)) => Some(*b),
                _ => None,
            })
            .unwrap_or(false)
    };
    let schema: Option<serde_json::Value> = s
        .and_then(|st| st.fields.get("schema"))
        .and_then(|v| serde_json::to_value(v).ok())
        .filter(|v| !v.is_null());
    SpawnAgentArgs {
        task: get_str("task").unwrap_or_default(),
        schema,
        context_files: None,
        detached: get_bool("detached"),
    }
}

/// Send a `Feedback` back to the brain over the Nerve.
async fn reply_feedback(
    writer: std::sync::Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    corr_id: u64,
    feedback: Feedback,
) {
    let resp = Signal {
        corr_id,
        body: Some(signal::Body::Feedback(feedback)),
    };
    let mut w = writer.lock().await;
    nerve::write_signal(&mut *w, &resp).await.ok();
    use tokio::io::AsyncWriteExt;
    w.flush().await.ok();
}

/// Launch a child `gait run` process from the spine.
///
/// Returns `Some(child)` for an enrolled subagent (retained for wait), or
/// `None` for a breakaway peer (detached into its own process group/cgroup).
fn launch_child(ctx: &TurnCtx, session_id: &str, detached: bool) -> Option<std::process::Child> {
    let mut argv = vec![
        "run".to_string(),
        "--session".to_string(),
        session_id.to_string(),
        "--gait-dir".to_string(),
        ctx.gait_dir.to_str().unwrap_or(".").to_string(),
        "--auto".to_string(),
    ];
    if ctx.sandbox_on {
        argv.push("--sandbox-locked".to_string());
    }
    if detached {
        let mut cmd = std::process::Command::new(&ctx.exe);
        cmd.args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let _ = ctx.group.spawn_detached(cmd);
        None
    } else {
        // Subagent: enroll in cascade group.
        let handle = ctx.group.handle();
        if let Some(h) = handle {
            argv.extend(["--cascade-group".to_string(), h]);
        } else {
            eprintln!("[spine] warning: spawn_agent cascade-group handle unavailable; subagent will not be enrolled in cascade group");
        }
        let mut cmd = std::process::Command::new(&ctx.exe);
        cmd.args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        ctx.group.spawn_enrolled(cmd).ok()
    }
}

/// Reads the child's session log and returns the last `TurnComplete`'s
/// `(message, result)`. Returns `None` if no `TurnComplete` event is found.
fn read_terminal_result(log_path: &std::path::Path) -> Option<(String, Option<serde_json::Value>)> {
    use crate::proto::gait::log::v1::payload;
    let events = crate::session::log::read_events(log_path).ok()?;
    // Walk backwards to find the last TurnComplete.
    for event in events.iter().rev() {
        if let Some(payload::Kind::TurnComplete(tc)) = crate::proto::event_kind(event) {
            let output = tc.output.as_ref()?;
            let message = output.message.clone();
            // Convert pbjson_types::Value → serde_json::Value via serde round-trip.
            let result: Option<serde_json::Value> = output
                .result
                .as_ref()
                .and_then(|v| serde_json::to_value(v).ok())
                .filter(|v| !v.is_null());
            return Some((message, result));
        }
    }
    None
}

/// Spine-side handler for `spawn_agent`: creates the child session (disk writes)
/// and launches the child process. Returns a `Feedback` with `{session_id}`.
pub(super) async fn handle_spawn_agent(args: SpawnAgentArgs, ctx: TurnCtx) -> Feedback {
    use crate::tools::spawn_agent::{write_child_session, WriteChildSessionOpts};

    let new_id = crate::session::new_session_id();

    if write_child_session(WriteChildSessionOpts {
        gait_dir: &ctx.gait_dir,
        codebase: &ctx.codebase,
        new_id: &new_id,
        parent_session_id: &ctx.session_id,
        task: args.task,
        notify_done: args.detached,
        repos: &ctx.repos,
    })
    .is_err()
    {
        return error_feedback("spawn_agent: failed to create child session");
    }

    // Persist per-spawn schema so the child CLI can read it into ProfileInputs.
    if let Some(ref schema) = args.schema {
        let schema_path = ctx
            .gait_dir
            .join("sessions")
            .join(&new_id)
            .join("result_schema.json");
        match serde_json::to_vec_pretty(schema) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&schema_path, &bytes) {
                    eprintln!("[spine] warning: failed to write result_schema.json: {e}");
                }
            }
            Err(e) => {
                eprintln!("[spine] warning: failed to serialize result schema: {e}");
            }
        }
    }

    if args.detached {
        // Fire-and-forget peer: break out of the cascade group, don't wait.
        let _ = launch_child(&ctx, &new_id, true);
        return feedback_result(serde_json::json!({ "session_id": new_id }));
    }

    // Blocking subagent: wait for the child process to exit, then read its result.
    let tx_path = ctx
        .gait_dir
        .join("sessions")
        .join(&new_id)
        .join("log.jsonl");

    let Some(mut child) = launch_child(&ctx, &new_id, false) else {
        return error_feedback("spawn_agent: failed to launch subagent");
    };

    // Await child exit without blocking a tokio worker thread.
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .ok() // JoinError → None
        .and_then(|r| r.ok()); // io::Error → None

    // Check if killed by signal (cascade kill = parent cancelled the session).
    let was_signalled = status
        .as_ref()
        .map(|s| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                s.signal().is_some()
            }
            #[cfg(not(unix))]
            {
                false
            }
        })
        .unwrap_or(false);

    if was_signalled {
        return feedback_result(serde_json::json!({
            "session_id": new_id,
            "status": "cancelled",
        }));
    }

    let exit_code = status.as_ref().and_then(|s| s.code()).unwrap_or(-1);

    build_subagent_feedback(&new_id, &tx_path, exit_code)
}

/// Assemble a `Feedback` after the blocking subagent exits.
///
/// Reads the child's session log for a `TurnComplete` event. Extracted as a
/// separate function so it can be unit-tested independently of process launching.
pub(super) fn build_subagent_feedback(
    session_id: &str,
    log_path: &std::path::Path,
    exit_code: i32,
) -> Feedback {
    match read_terminal_result(log_path) {
        Some((message, result)) => feedback_result(serde_json::json!({
            "session_id": session_id,
            "status": "done",
            "message": message,
            "result": result,
        })),
        None => feedback_result(serde_json::json!({
            "session_id": session_id,
            "status": "failed",
            "summary": format!("subagent exited (code {exit_code}) without completing"),
        })),
    }
}

/// Arguments parsed from a `run_workflow` Impulse.
pub(super) struct RunWorkflowArgs {
    pub name: String,
    pub wf_args: Vec<String>,
    pub detached: bool,
}

/// Parse a proto `Struct` from an Impulse into `RunWorkflowArgs`.
fn parse_run_workflow_args(impulse: &Impulse) -> RunWorkflowArgs {
    let s = impulse.args.as_ref();
    let get_str = |key: &str| -> Option<String> {
        s.and_then(|st| st.fields.get(key))
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.clone()),
                _ => None,
            })
    };
    let get_bool = |key: &str| -> bool {
        s.and_then(|st| st.fields.get(key))
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::BoolValue(b)) => Some(*b),
                _ => None,
            })
            .unwrap_or(false)
    };
    let wf_args: Vec<String> = s
        .and_then(|st| st.fields.get("args"))
        .and_then(|v| match &v.kind {
            Some(pbjson_types::value::Kind::ListValue(lv)) => Some(
                lv.values
                    .iter()
                    .filter_map(|v| match &v.kind {
                        Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    RunWorkflowArgs {
        name: get_str("name").unwrap_or_default(),
        wf_args,
        detached: get_bool("detached"),
    }
}

/// Spine-side handler for `run_workflow`: launches `gait workflow run <name>` as a
/// subprocess (enrolled or detached) and waits for it to finish (unless `detached`).
pub(super) async fn handle_run_workflow(args: RunWorkflowArgs, ctx: TurnCtx) -> Feedback {
    if args.name.is_empty() {
        return error_feedback("run_workflow: missing required field: name");
    }

    if args.detached {
        let _ = launch_workflow_child(&ctx, &args.name, &args.wf_args, true);
        return feedback_result(serde_json::json!({
            "status": "started",
            "name": args.name,
        }));
    }

    // Blocking: wait for the workflow process to exit, then read the run log.
    let Some(mut child) = launch_workflow_child(&ctx, &args.name, &args.wf_args, false) else {
        return error_feedback("run_workflow: failed to launch workflow subprocess");
    };

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .ok()
        .and_then(|r| r.ok());

    let was_signalled = status
        .as_ref()
        .map(|s| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                s.signal().is_some()
            }
            #[cfg(not(unix))]
            {
                false
            }
        })
        .unwrap_or(false);

    if was_signalled {
        return feedback_result(serde_json::json!({
            "name": args.name,
            "status": "cancelled",
        }));
    }

    let exit_code = status.as_ref().and_then(|s| s.code()).unwrap_or(-1);
    build_workflow_feedback(&args.name, exit_code)
}

/// Launch `gait workflow run <name> [args…] --gait-dir … [--sandbox-locked] [--cascade-group …]`
/// as a child process (enrolled or detached).
fn launch_workflow_child(
    ctx: &TurnCtx,
    name: &str,
    wf_args: &[String],
    detached: bool,
) -> Option<std::process::Child> {
    let mut argv = vec![
        "workflow".to_string(),
        "run".to_string(),
        name.to_string(),
        "--gait-dir".to_string(),
        ctx.gait_dir.to_str().unwrap_or(".").to_string(),
    ];
    for a in wf_args {
        argv.push(a.clone());
    }
    if ctx.sandbox_on {
        argv.push("--sandbox-locked".to_string());
    }
    if detached {
        let mut cmd = std::process::Command::new(&ctx.exe);
        cmd.args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let _ = ctx.group.spawn_detached(cmd);
        None
    } else {
        let handle = ctx.group.handle();
        if let Some(h) = handle {
            argv.extend(["--cascade-group".to_string(), h]);
        } else {
            eprintln!("[spine] warning: run_workflow cascade-group handle unavailable");
        }
        let mut cmd = std::process::Command::new(&ctx.exe);
        cmd.args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        ctx.group.spawn_enrolled(cmd).ok()
    }
}

/// Assemble a `Feedback` after the blocking workflow subprocess exits.
///
/// Exit-code mapping:
///   0  → `"done"`            (`WorkflowOutcome::Completed`)
///   2  → `"paused_at_gate"`  (`WorkflowOutcome::GatePaused`)
///   other → `"failed"`
///
/// NOTE(SP-2c): the brain has only the workflow *name*; the subprocess re-reads
/// `{gait_dir}/workflows/{name}.yaml` — acceptable, but does not exercise embedded-def resume.
pub(super) fn build_workflow_feedback(name: &str, exit_code: i32) -> Feedback {
    match exit_code {
        0 => feedback_result(serde_json::json!({
            "name": name,
            "status": "done",
        })),
        2 => feedback_result(serde_json::json!({
            "name": name,
            "status": "paused_at_gate",
        })),
        _ => feedback_result(serde_json::json!({
            "name": name,
            "status": "failed",
            "summary": format!("workflow subprocess exited with code {exit_code}"),
        })),
    }
}

#[cfg(test)]
pub(super) fn test_ctx(ws: &std::path::Path, session_id: &str) -> TurnCtx {
    TurnCtx {
        gait_dir: ws.to_path_buf(),
        codebase: ws.to_path_buf(),
        session_id: session_id.to_string(),
        repos: crate::git::Repos::production(),
        exe: std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("gait")),
        group: crate::process::establish(None),
        sandbox_on: false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        brain_argv, build_subagent_feedback, build_turn_profile, enrich_permissions_preview,
        error_feedback, handle_spawn_agent, test_ctx, watchdog_breach, ProfileInputs,
        SpawnAgentArgs, WatchdogCfg,
    };

    #[test]
    fn brain_argv_is_canonical_and_has_no_role_token() {
        let profile = ProfileInputs {
            agent_name: Some("default".into()),
            model_override: Some("anthropic/x".into()),
            allow: vec!["Bash".into()],
            deny: vec!["Net".into()],
            result_schema: None,
        };
        let argv = brain_argv(
            std::path::Path::new("/g"),
            std::path::Path::new("/c"),
            "s123",
            true,
            Some(7),
            true,
            &profile,
        );
        // The role token is added by the spawn, never by the builder.
        assert!(!argv.iter().any(|a| a == "__brain"));
        // Canonical required flags.
        assert_eq!(argv[0], "--gait-dir");
        assert_eq!(argv[1], "/g");
        assert!(windowed(&argv, "--codebase", "/c"));
        assert!(windowed(&argv, "--session", "s123"));
        assert!(argv.iter().any(|a| a == "--auto"));
        assert!(windowed(&argv, "--max-iter", "7"));
        assert!(windowed(&argv, "--allow", "Bash"));
        assert!(windowed(&argv, "--deny", "Net"));
        // Sealed spine ⇒ sealed brain.
        assert!(argv.iter().any(|a| a == "--sandbox-locked"));
        // model/agent/result-schema flow via BrainInit, NOT argv.
        assert!(!argv.iter().any(|a| a == "--model"));
        assert!(!argv.iter().any(|a| a == "--agent"));
    }

    #[test]
    fn brain_argv_unsandboxed_emits_no_sandbox() {
        let profile = ProfileInputs {
            agent_name: None,
            model_override: None,
            allow: vec![],
            deny: vec![],
            result_schema: None,
        };
        let argv = brain_argv(
            std::path::Path::new("/g"),
            std::path::Path::new("/c"),
            "s",
            false,
            None,
            false,
            &profile,
        );
        assert!(argv.iter().any(|a| a == "--no-sandbox"));
        assert!(!argv.iter().any(|a| a == "--sandbox-locked"));
        assert!(!argv.iter().any(|a| a == "--auto"));
        assert!(!argv.iter().any(|a| a == "--max-iter"));
    }

    // helper: true if `flag` is immediately followed by `val` in argv.
    fn windowed(argv: &[String], flag: &str, val: &str) -> bool {
        argv.windows(2).any(|w| w[0] == flag && w[1] == val)
    }

    #[tokio::test]
    async fn handle_spawn_agent_creates_session_and_returns_id() {
        let ws = tempfile::TempDir::new().unwrap();
        crate::test_utils::init_git_repo(ws.path());
        std::fs::write(
            ws.path().join("gait.jsonc"),
            r#"{"model":"minimax/MiniMax-M3","allow":[],"tools":[]}"#,
        )
        .unwrap();
        let fb = handle_spawn_agent(
            SpawnAgentArgs {
                task: "do X".into(),
                schema: None,
                context_files: None,
                detached: true,
            },
            test_ctx(ws.path(), "parent01"),
        )
        .await;
        // result is a proto Struct; extract session_id directly as a StringValue field.
        let id = fb
            .result
            .as_ref()
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StructValue(s)) => s.fields.get("session_id"),
                _ => None,
            })
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(
            ws.path().join(format!("sessions/{id}/log.jsonl")).exists(),
            "child session log must exist at sessions/{id}/log.jsonl"
        );
    }

    /// When `spawn_agent` is called with a non-null `schema`, the schema must be
    /// persisted to `sessions/<child_id>/result_schema.json` in the gait-dir so the
    /// child CLI can load it into `ProfileInputs.result_schema`.
    ///
    /// A full end-to-end test (parent spawns child with schema → child calls
    /// `structured_done` → parent receives result) requires a live LLM API and a real
    /// `gait` binary in PATH; that is not feasible in unit/integration tests. Instead we
    /// exercise the two independently-testable halves:
    ///   1. `handle_spawn_agent` writes `result_schema.json` (this test).
    ///   2. `build_subagent_feedback` reads a pre-written `TurnComplete` carrying
    ///      `result:{answer:1}` and returns it in the Feedback (next test).
    #[tokio::test]
    async fn handle_spawn_agent_with_schema_writes_result_schema_json() {
        let ws = tempfile::TempDir::new().unwrap();
        crate::test_utils::init_git_repo(ws.path());
        std::fs::write(
            ws.path().join("gait.jsonc"),
            r#"{"model":"minimax/MiniMax-M3","allow":[],"tools":[]}"#,
        )
        .unwrap();
        let schema = serde_json::json!({"type": "object", "required": ["answer"]});
        let fb = handle_spawn_agent(
            SpawnAgentArgs {
                task: "answer the question".into(),
                schema: Some(schema.clone()),
                context_files: None,
                detached: true, // fire-and-forget so we don't need a real binary
            },
            test_ctx(ws.path(), "parent-schema-01"),
        )
        .await;
        // Extract the child session id from the returned Feedback.
        let id = fb
            .result
            .as_ref()
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StructValue(s)) => s.fields.get("session_id"),
                _ => None,
            })
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.clone()),
                _ => None,
            })
            .expect("Feedback must carry session_id");
        let schema_path = ws
            .path()
            .join("sessions")
            .join(&id)
            .join("result_schema.json");
        assert!(
            schema_path.exists(),
            "result_schema.json must exist at {schema_path:?}"
        );
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&schema_path).unwrap()).unwrap();
        assert_eq!(
            on_disk, schema,
            "result_schema.json content must round-trip the schema"
        );
    }

    /// Asserts that `build_subagent_feedback` correctly surfaces `result:{answer:1}`
    /// from a child log that contains a `TurnComplete` written by `structured_done`.
    ///
    /// This covers the Task-4 requirement: "assert the parent's `spawn_agent` result
    /// carries `{answer:1}`".  The live end-to-end path (real LLM → real child process)
    /// is not feasible without a live API key and a compiled `gait` binary on PATH.
    #[test]
    fn build_subagent_feedback_result_answer_1() {
        use crate::proto::events::{turn_complete, TurnCompleteParams};
        use crate::proto::wire;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");

        // Simulate the child calling `structured_done(result:{answer:1})`.
        wire::append(
            &log_path,
            turn_complete(TurnCompleteParams {
                message: "done".into(),
                result: Some(serde_json::json!({"answer": 1})),
                ..Default::default()
            }),
        )
        .unwrap();

        let fb = build_subagent_feedback("child-answer-1", &log_path, 0);

        let fields = match fb.result.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => &s.fields,
            other => panic!("expected StructValue result, got {other:?}"),
        };

        // status == "done"
        let status = fields
            .get("status")
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .expect("status field must be present");
        assert_eq!(status, "done");

        // result.answer == 1
        let result_struct = match fields.get("result").and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => s,
            other => panic!("expected result StructValue, got {other:?}"),
        };
        let answer = match result_struct
            .fields
            .get("answer")
            .and_then(|v| v.kind.as_ref())
        {
            Some(pbjson_types::value::Kind::NumberValue(n)) => *n,
            other => panic!("expected NumberValue for answer, got {other:?}"),
        };
        assert!(
            (answer - 1.0).abs() < f64::EPSILON,
            "spawn_agent result must carry answer:1, got {answer}"
        );
    }

    #[test]
    fn hand_response_error_arm_becomes_error_feedback() {
        use gait_proto::gait::sandbox::v1::{hand_response, HandError, HandResponse};
        use prost::Message as _;
        let bytes = HandResponse {
            outcome: Some(hand_response::Outcome::Error(HandError {
                message: "nope".into(),
            })),
        }
        .encode_to_vec();
        // mirror the spine's decode mapping
        let fb = match HandResponse::decode(bytes.as_slice()) {
            Ok(HandResponse {
                outcome: Some(hand_response::Outcome::Error(e)),
            }) => error_feedback(&format!("hand error: {}", e.message)),
            _ => panic!("expected error arm"),
        };
        let s = fb.result.and_then(|v| match v.kind {
            Some(pbjson_types::value::Kind::StringValue(s)) => Some(s),
            _ => None,
        });
        assert_eq!(s.as_deref(), Some("hand error: nope"));
    }

    #[test]
    fn build_turn_profile_threads_result_schema() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("gait.jsonc"), r#"{"model":"m"}"#).unwrap();
        let schema = serde_json::json!({"type":"object","required":["answer"]});
        let inputs = ProfileInputs {
            agent_name: None,
            model_override: None,
            allow: vec![],
            deny: vec![],
            result_schema: Some(schema.clone()),
        };
        let p = build_turn_profile(ws.path(), ws.path(), &inputs);
        assert_eq!(p.result_schema, Some(schema));
    }

    #[test]
    fn build_turn_profile_folds_cli_allow() {
        let dir = tempfile::TempDir::new().unwrap();
        // Write a minimal gait.jsonc so load_config picks up a known model.
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"anthropic/x"}"#).unwrap();
        let inputs = ProfileInputs {
            agent_name: None,
            model_override: None,
            allow: vec!["read".into()],
            deny: vec![],
            result_schema: None,
        };
        let p = build_turn_profile(dir.path(), dir.path(), &inputs);
        assert_eq!(p.model, "anthropic/x");
        assert!(p.allow.iter().any(|a| a == "read"));
    }

    #[test]
    #[should_panic(expected = "model must be set before building Profile")]
    fn build_turn_profile_panics_without_config_or_override() {
        let dir = tempfile::TempDir::new().unwrap();
        // No gait.jsonc — load_config falls back to GaitConfig::default() which has model: None.
        let inputs = ProfileInputs {
            agent_name: None,
            model_override: None,
            allow: vec![],
            deny: vec![],
            result_schema: None,
        };
        // Should panic: no model in config and no model_override supplied.
        let _ = build_turn_profile(dir.path(), dir.path(), &inputs);
    }

    fn make_permissions_event(
        items: Vec<crate::proto::gait::log::v1::PermissionItem>,
    ) -> crate::proto::gait::log::v1::Event {
        use crate::proto::gait::log::v1::{event, payload, Payload, Permissions};
        gait_proto::envelope::mint(
            event::Body::Payload(Payload {
                kind: Some(payload::Kind::Permissions(Permissions { items })),
            }),
            None,
            0,
            0,
        )
    }

    /// An in-scope write target: preview should be filled.
    #[test]
    fn enrich_fills_preview_for_in_scope_write() {
        use crate::config::SandboxConfig;
        use crate::proto::gait::log::v1::{event, payload, PermissionItem};

        let dir = tempfile::TempDir::new().unwrap();
        let codebase = dir.path();
        let target_file = codebase.join("hello.txt");
        std::fs::write(&target_file, "old content\n").unwrap();

        let item = PermissionItem {
            id: "req-1".into(),
            cmd: "write hello.txt".into(),
            reason: "write requires permission".into(),
            patterns: vec!["write:hello.txt".into()],
            tool_name: "write".into(),
            tool_args: crate::proto::events::to_struct(&serde_json::json!({
                "file_path": target_file.to_string_lossy(),
                "content": "new content\n"
            })),
            preview: None,
        };

        let mut evt = make_permissions_event(vec![item]);
        let sandbox_cfg = SandboxConfig::default();
        enrich_permissions_preview(&mut evt, codebase, codebase, "s1", &sandbox_cfg);

        let Some(event::Body::Payload(p)) = evt.body.as_ref() else {
            panic!("expected payload body");
        };
        let Some(payload::Kind::Permissions(perm)) = p.kind.as_ref() else {
            panic!("expected permissions");
        };
        assert_eq!(perm.items.len(), 1);
        assert!(
            perm.items[0].preview.is_some(),
            "expected preview to be filled for in-scope write"
        );
    }

    /// A target outside the codebase: preview should stay None.
    #[test]
    fn enrich_skips_preview_for_out_of_scope_target() {
        use crate::config::SandboxConfig;
        use crate::proto::gait::log::v1::{event, payload, PermissionItem};

        let dir = tempfile::TempDir::new().unwrap();
        let codebase = dir.path();

        // Point the write outside the codebase (e.g. /etc/passwd)
        let item = PermissionItem {
            id: "req-2".into(),
            cmd: "write /etc/passwd".into(),
            reason: "write requires permission".into(),
            patterns: vec!["write:/etc/passwd".into()],
            tool_name: "write".into(),
            tool_args: crate::proto::events::to_struct(&serde_json::json!({
                "file_path": "/etc/passwd",
                "content": "malicious"
            })),
            preview: None,
        };

        let mut evt = make_permissions_event(vec![item]);
        let sandbox_cfg = SandboxConfig::default();
        enrich_permissions_preview(&mut evt, codebase, codebase, "s1", &sandbox_cfg);

        let Some(event::Body::Payload(p)) = evt.body.as_ref() else {
            panic!("expected payload body");
        };
        let Some(payload::Kind::Permissions(perm)) = p.kind.as_ref() else {
            panic!("expected permissions");
        };
        assert!(
            perm.items[0].preview.is_none(),
            "expected preview to be None for out-of-scope target"
        );
    }

    /// A tool name that doesn't exist in builtin_tools → find returns None → continue →
    /// preview stays None. Exercises the empty-targets short-circuit guard.
    #[test]
    fn enrich_skips_preview_when_no_targets() {
        use crate::config::SandboxConfig;
        use crate::proto::gait::log::v1::{event, payload, PermissionItem};

        let dir = tempfile::TempDir::new().unwrap();
        let codebase = dir.path();

        // Use a tool name that doesn't exist in builtin_tools so the find() returns None
        // and the loop continues without touching preview.
        let item = PermissionItem {
            id: "req-no-targets".into(),
            cmd: "nonexistent_tool".into(),
            reason: "some reason".into(),
            patterns: vec![],
            tool_name: "nonexistent_tool_xyz".into(),
            tool_args: None,
            preview: None,
        };

        let mut evt = make_permissions_event(vec![item]);
        let sandbox_cfg = SandboxConfig::default();
        enrich_permissions_preview(&mut evt, codebase, codebase, "s1", &sandbox_cfg);

        let Some(event::Body::Payload(p)) = evt.body.as_ref() else {
            panic!("expected payload body");
        };
        let Some(payload::Kind::Permissions(perm)) = p.kind.as_ref() else {
            panic!("expected permissions kind");
        };
        assert!(
            perm.items[0].preview.is_none(),
            "expected preview to remain None when tool has no targets"
        );
    }

    /// Already-filled preview should not be overwritten.
    #[test]
    fn enrich_does_not_overwrite_existing_preview() {
        use crate::config::SandboxConfig;
        use crate::proto::gait::log::v1::{event, payload, PermissionItem};

        let dir = tempfile::TempDir::new().unwrap();
        let codebase = dir.path();
        let target_file = codebase.join("existing.txt");
        std::fs::write(&target_file, "content\n").unwrap();

        let item = PermissionItem {
            id: "req-3".into(),
            cmd: "write existing.txt".into(),
            reason: "write requires permission".into(),
            patterns: vec![],
            tool_name: "write".into(),
            tool_args: crate::proto::events::to_struct(&serde_json::json!({
                "file_path": target_file.to_string_lossy(),
                "content": "new\n"
            })),
            preview: Some("--- brain computed this ---".into()),
        };

        let mut evt = make_permissions_event(vec![item]);
        let sandbox_cfg = SandboxConfig::default();
        enrich_permissions_preview(&mut evt, codebase, codebase, "s1", &sandbox_cfg);

        let Some(event::Body::Payload(p)) = evt.body.as_ref() else {
            panic!("expected payload body in overwrite test");
        };
        let Some(payload::Kind::Permissions(perm)) = p.kind.as_ref() else {
            panic!("expected permissions kind in overwrite test");
        };
        assert_eq!(
            perm.items[0].preview.as_deref(),
            Some("--- brain computed this ---"),
            "existing preview must not be overwritten"
        );
    }

    /// A cancelled turn with an orphan ToolCall leaves a synthesized ToolResult
    /// in the log, so retrace::messages reconstructs with no orphan.
    ///
    /// This test exercises the synthesis logic directly: write a ToolCall to the
    /// log, then simulate what the spine's teardown does (drain pending_calls and
    /// append synthetic ToolResults), and verify the log ends with a matching
    /// ToolResult and retrace sees no orphan tool calls.
    #[test]
    fn cancelled_turn_synthesizes_tool_result_for_orphan_call_id() {
        use crate::proto::gait::log::v1::{event, payload};
        use crate::proto::{envelope, wire};
        use crate::session::retrace;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");

        // 1. Write a ToolCall (simulating what the brain emits mid-turn).
        let call_id = "call-orphan-1".to_string();
        let tc_body = crate::proto::events::tool_call(
            call_id.clone(),
            "read".into(),
            &serde_json::json!({"file_path": "foo.rs"}),
        );
        wire::append(&log_path, tc_body).expect("append tool_call");

        // 2. Simulate cancel: the spine synthesizes a ToolResult for the orphan.
        let body = crate::proto::events::tool_result(
            call_id.clone(),
            "cancelled by user".into(),
            None,
            None,
        );
        let prev = crate::session::log::last_event(&log_path).ok().flatten();
        let ev = envelope::mint(body, prev.as_ref(), 0, 0);
        wire::append_with_id(&log_path, &ev).expect("append synthesized tool_result");

        // 3. Assert: the last event is a ToolResult with the correct call_id.
        let events = crate::session::log::read_events(&log_path).unwrap();
        assert_eq!(events.len(), 2, "log must have exactly 2 events");
        let last = events.last().unwrap();
        match last.body.as_ref() {
            Some(event::Body::Payload(p)) => match p.kind.as_ref() {
                Some(payload::Kind::ToolResult(tr)) => {
                    assert_eq!(
                        tr.call_id, call_id,
                        "synthesized ToolResult must match call_id"
                    );
                    assert_eq!(tr.output, "cancelled by user");
                }
                other => panic!("expected ToolResult, got {other:?}"),
            },
            other => panic!("expected Payload body, got {other:?}"),
        }

        // 4. Assert: retrace::messages reconstructs with no orphan tool calls.
        // Without the synthesized ToolResult, retrace would produce a ToolCall
        // message with no matching ToolResult (an orphan). With it, it should
        // produce a balanced assistant message followed by a tool result message.
        let messages = retrace::messages(&events).expect("retrace must succeed");
        // Every ToolCall in a messages list must have a corresponding ToolResult.
        // retrace folds them into paired assistant (tool_calls) + user (tool_results)
        // messages. Verify no dangling tool_call_id in the assistant message.
        let has_orphan = messages.iter().any(|m| {
            use crate::types::Message;
            match m {
                Message::Assistant { content, .. } => {
                    use crate::types::AssistantContent;
                    matches!(content, AssistantContent::Parts(parts) if {
                        parts.iter().any(|p| {
                            use crate::types::AssistantPart;
                            matches!(p, AssistantPart::ToolCall(_))
                        })
                    }) && !messages.iter().any(|m2| {
                        matches!(m2, Message::ToolResult { content, .. } if {
                            content.iter().any(|r| r.tool_call_id == call_id)
                        })
                    })
                }
                _ => false,
            }
        });
        assert!(
            !has_orphan,
            "retrace must find no orphan ToolCall after synthesis"
        );
    }

    // ── read_terminal_result tests ───────────────────────────────────────────

    /// Log with a TurnComplete event → returns (message, result).
    #[test]
    fn read_terminal_result_finds_turn_complete() {
        use crate::proto::events::{turn_complete, TurnCompleteParams};
        use crate::proto::wire;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");

        let body = turn_complete(TurnCompleteParams {
            message: "hello from child".into(),
            result: Some(serde_json::json!({"score": 42})),
            ..Default::default()
        });
        wire::append(&log_path, body).unwrap();

        let result = super::read_terminal_result(&log_path);
        assert!(result.is_some(), "should find turn_complete");
        let (msg, val) = result.unwrap();
        assert_eq!(msg, "hello from child");
        let val = val.expect("result should be Some");
        // pbjson_types::Value serializes numbers as f64, so 42 becomes 42.0.
        let score = val["score"].as_f64().expect("score should be a number");
        assert!(
            (score - 42.0).abs() < f64::EPSILON,
            "score should be 42, got {score}"
        );
    }

    /// Empty log → None.
    #[test]
    fn read_terminal_result_empty_log_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");
        // Don't write anything.
        let result = super::read_terminal_result(&log_path);
        assert!(result.is_none(), "empty log should return None");
    }

    /// Log with events but no TurnComplete → None.
    #[test]
    fn read_terminal_result_no_turn_complete_returns_none() {
        use crate::proto::wire;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");
        // Write only a UserText event.
        let body = crate::proto::events::user_text("some task".into(), None);
        wire::append(&log_path, body).unwrap();

        let result = super::read_terminal_result(&log_path);
        assert!(result.is_none(), "no turn_complete should return None");
    }

    /// handle_spawn_agent with detached=false but child cannot be launched (bad exe) →
    /// no process but session exists. We can't easily test child.wait() path without a
    /// real gait binary, so test the log-read fallback: write a session with no TurnComplete
    /// and verify a hypothetical `read_terminal_result` returns None (status == "failed").
    #[test]
    fn read_terminal_result_last_turn_complete_wins() {
        use crate::proto::events::{turn_complete, TurnCompleteParams};
        use crate::proto::wire;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");

        // Write two TurnComplete events; the second should win.
        wire::append(
            &log_path,
            turn_complete(TurnCompleteParams {
                message: "first".into(),
                ..Default::default()
            }),
        )
        .unwrap();
        wire::append(
            &log_path,
            turn_complete(TurnCompleteParams {
                message: "second".into(),
                result: Some(serde_json::json!("final")),
                ..Default::default()
            }),
        )
        .unwrap();

        let (msg, val) = super::read_terminal_result(&log_path).unwrap();
        assert_eq!(msg, "second");
        assert_eq!(val.unwrap(), serde_json::json!("final"));
    }

    /// Subagent (enrolled) is killed by kill_all; peer (detached) survives.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn launch_child_subagent_enrolled_peer_breaks_away() {
        use crate::process::establish;

        // Build a minimal TurnCtx with a real containment group.
        let ws = tempfile::TempDir::new().unwrap();
        let group = establish(None);

        // We'll use `sleep 60` as a stand-in for gait run — the exe path doesn't
        // matter because we're testing containment behavior, not gait startup.
        let sleep_exe = std::path::PathBuf::from("sleep");

        let ctx = super::TurnCtx {
            gait_dir: ws.path().to_path_buf(),
            codebase: ws.path().to_path_buf(),
            session_id: "parent".to_string(),
            repos: crate::git::Repos::production(),
            exe: sleep_exe,
            group: group.clone(),
            sandbox_on: false,
        };

        // Spawn the "enrolled" subagent: launch_child with detached=false.
        // We pass a dummy session_id; since the exe is `sleep`, args after `run`
        // are ignored — sleep just reads arg[1] as duration.  We can't fully
        // exercise the gait CLI here, so we test containment membership directly
        // by spawning sleep via the group APIs and verifying kill_all behavior.

        // Enrolled child via the group directly (mirrors what launch_child does).
        let mut enrolled_cmd = std::process::Command::new("sleep");
        enrolled_cmd
            .arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut enrolled = ctx
            .group
            .spawn_enrolled(enrolled_cmd)
            .expect("spawn enrolled");
        let enrolled_pid = enrolled.id() as i32;

        // Detached (peer) child via the group directly (mirrors what launch_child does).
        let mut peer_cmd = std::process::Command::new("sleep");
        peer_cmd
            .arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let (mut peer, peer_pid) = ctx.group.spawn_detached(peer_cmd).expect("spawn detached");

        // Kill all enrolled processes.
        ctx.group.kill_all().expect("kill_all");
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Reap the enrolled child so it's not a zombie.
        let _ = enrolled.wait();

        // Enrolled child must be dead.
        let enrolled_alive = unsafe { libc::kill(enrolled_pid, 0) };
        // Peer must still be alive.
        let peer_alive = unsafe { libc::kill(peer_pid, 0) };

        // Clean up peer.
        unsafe { libc::kill(peer_pid, libc::SIGKILL) };
        let _ = peer.wait();

        assert_eq!(
            enrolled_alive, -1,
            "enrolled subagent (pid {enrolled_pid}) should be dead after kill_all"
        );
        assert_eq!(
            peer_alive, 0,
            "breakaway peer (pid {peer_pid}) should survive kill_all"
        );
    }

    // ── build_subagent_feedback integration tests ────────────────────────────

    /// Pre-write a `turn_complete` with `result = {"answer": 42}` to the child log,
    /// then call `build_subagent_feedback` and assert the returned Feedback carries
    /// the expected result value.
    #[test]
    fn build_subagent_feedback_reads_turn_complete_result() {
        use crate::proto::events::{turn_complete, TurnCompleteParams};
        use crate::proto::wire;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");

        // Pre-write the turn_complete event (as the child process would).
        wire::append(
            &log_path,
            turn_complete(TurnCompleteParams {
                message: "task done".into(),
                result: Some(serde_json::json!({"answer": 42})),
                ..Default::default()
            }),
        )
        .unwrap();

        let fb = build_subagent_feedback("child-session-1", &log_path, 0);

        // result is a proto Struct; navigate to the fields.
        let fields = match fb.result.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => &s.fields,
            other => panic!("expected StructValue, got {other:?}"),
        };

        // status == "done"
        let status = fields.get("status").and_then(|v| match &v.kind {
            Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
            _ => None,
        });
        assert_eq!(status, Some("done"), "status must be 'done'");

        // message == "task done"
        let message = fields.get("message").and_then(|v| match &v.kind {
            Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.clone()),
            _ => None,
        });
        assert_eq!(message.as_deref(), Some("task done"));

        // result.answer == 42 (stored as f64 in proto Number)
        let result_struct = match fields.get("result").and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => s,
            other => panic!("expected result StructValue, got {other:?}"),
        };
        let answer = match result_struct
            .fields
            .get("answer")
            .and_then(|v| v.kind.as_ref())
        {
            Some(pbjson_types::value::Kind::NumberValue(n)) => *n,
            other => panic!("expected NumberValue for answer, got {other:?}"),
        };
        assert!(
            (answer - 42.0).abs() < f64::EPSILON,
            "answer must be 42, got {answer}"
        );
    }

    /// When the child log has no `TurnComplete`, `build_subagent_feedback` should
    /// return a Feedback with `status == "failed"`.
    #[test]
    fn build_subagent_feedback_no_turn_complete_returns_failed() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");
        // Do NOT write any events — simulates child that exited without completing.

        let fb = build_subagent_feedback("child-session-2", &log_path, 1);

        let fields = match fb.result.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => &s.fields,
            other => panic!("expected StructValue, got {other:?}"),
        };

        let status = fields.get("status").and_then(|v| match &v.kind {
            Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
            _ => None,
        });
        assert_eq!(status, Some("failed"), "status must be 'failed'");
    }

    // ── run_workflow relay-routing tests ──────────────────────────────────────

    /// `build_workflow_feedback` with exit_code 0 → `status == "done"`.
    #[test]
    fn build_workflow_feedback_exit_0_is_done() {
        let fb = super::build_workflow_feedback("my-flow", 0);
        let fields = match fb.result.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => &s.fields,
            other => panic!("expected StructValue, got {other:?}"),
        };
        let status = fields
            .get("status")
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .expect("status field missing");
        assert_eq!(status, "done");
        let name = fields
            .get("name")
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .expect("name field missing");
        assert_eq!(name, "my-flow");
    }

    /// `build_workflow_feedback` with non-zero exit → `status == "failed"`.
    #[test]
    fn build_workflow_feedback_nonzero_exit_is_failed() {
        let fb = super::build_workflow_feedback("my-flow", 1);
        let fields = match fb.result.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => &s.fields,
            other => panic!("expected StructValue, got {other:?}"),
        };
        let status = fields
            .get("status")
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .expect("status field missing");
        assert_eq!(status, "failed");
    }

    /// `handle_run_workflow` with missing name → error feedback.
    #[tokio::test]
    async fn handle_run_workflow_empty_name_returns_error() {
        let ws = tempfile::TempDir::new().unwrap();
        let fb = super::handle_run_workflow(
            super::RunWorkflowArgs {
                name: String::new(),
                wf_args: vec![],
                detached: false,
            },
            test_ctx(ws.path(), "parent-wf"),
        )
        .await;
        // error_feedback puts the message string at the top-level result.
        let is_error = matches!(
            fb.result.as_ref().and_then(|v| v.kind.as_ref()),
            Some(pbjson_types::value::Kind::StringValue(_))
        );
        assert!(is_error, "expected error string feedback for empty name");
    }

    /// `build_workflow_feedback` with exit code 2 → `status == "paused_at_gate"`.
    #[test]
    fn build_workflow_feedback_exit_2_is_paused_at_gate() {
        let fb = super::build_workflow_feedback("my-flow", 2);
        let fields = match fb.result.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => &s.fields,
            other => panic!("expected StructValue, got {other:?}"),
        };
        let status = fields
            .get("status")
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .expect("status field missing");
        assert_eq!(status, "paused_at_gate");
    }

    /// `handle_run_workflow` with `detached: false` (blocking relay path):
    /// verifies the non-detached code path is exercised and never returns `"started"`.
    /// In the test process context the cascade group has no leader yet so
    /// `spawn_enrolled` may fail; in that case an error-string feedback is expected.
    /// Either way the result must NOT be `"started"` (which is the detached return).
    #[tokio::test]
    async fn handle_run_workflow_blocking_returns_done_or_failed() {
        let ws = tempfile::TempDir::new().unwrap();
        // Use `/bin/true` as the exe — it accepts any argv and exits 0 immediately.
        let mut ctx = test_ctx(ws.path(), "parent-wf-blocking");
        ctx.exe = std::path::PathBuf::from("/bin/true");
        let fb = super::handle_run_workflow(
            super::RunWorkflowArgs {
                name: "smoke".into(),
                wf_args: vec![],
                detached: false,
            },
            ctx,
        )
        .await;
        // The result is either:
        //  - a StructValue with status == "done" | "failed" (if spawn succeeded)
        //  - a StringValue error message (if spawn_enrolled failed in test context)
        // In both cases it must NOT be a StructValue with status == "started".
        let is_started = match fb.result.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => {
                s.fields.get("status").and_then(|v| match &v.kind {
                    Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                    _ => None,
                }) == Some("started")
            }
            _ => false,
        };
        assert!(
            !is_started,
            "blocking run_workflow must never return status=started"
        );
    }

    /// `handle_run_workflow` detached with a real name → returns `status == "started"`.
    /// (exe is a dummy; spawn_detached is fire-and-forget, we just verify the feedback shape.)
    #[tokio::test]
    async fn handle_run_workflow_detached_returns_started() {
        let ws = tempfile::TempDir::new().unwrap();
        // Use a dummy exe path — spawn_detached is fire-and-forget; we only check feedback shape.
        let mut ctx = test_ctx(ws.path(), "parent-wf2");
        ctx.exe = std::path::PathBuf::from("false"); // exits immediately
        let fb = super::handle_run_workflow(
            super::RunWorkflowArgs {
                name: "smoke-test".into(),
                wf_args: vec![],
                detached: true,
            },
            ctx,
        )
        .await;
        let fields = match fb.result.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => &s.fields,
            other => panic!("expected StructValue, got {other:?}"),
        };
        let status = fields
            .get("status")
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .expect("status field missing");
        assert_eq!(status, "started");
    }

    #[test]
    fn diagnostic_line_is_appended_to_diagnostic_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path();
        let session = "stest";
        std::fs::create_dir_all(gait_dir.join("sessions").join(session)).unwrap();

        super::DiagnosticSink::new(gait_dir, session).terminal(r#"{"phase":"awaiting_headers"}"#);
        super::DiagnosticSink::new(gait_dir, session).terminal(r#"{"phase":"streaming"}"#);

        let p = gait_dir
            .join("sessions")
            .join(session)
            .join("diagnostic.jsonl");
        let body = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("awaiting_headers"));
        // And it did NOT touch log.jsonl
        assert!(!gait_dir
            .join("sessions")
            .join(session)
            .join("log.jsonl")
            .exists());
    }

    // ── Watchdog logic tests ──────────────────────────────────────────────────

    use crate::diagnostic::Phase;

    fn wd() -> WatchdogCfg {
        WatchdogCfg {
            stall_cap_ms: 90_000,
            ping_ms: 15_000,
            absolute_backstop_ms: 660_000,
        }
    }

    #[test]
    fn phase_elapsed_cap_fires_while_alive() {
        // Fast phase past stall_cap is a breach EVEN with a fresh ping.
        assert!(
            watchdog_breach(&wd(), Phase::AwaitingHeaders, 95_000, Some(1_000), false).is_some()
        );
    }

    #[test]
    fn no_ping_cap_fires_when_wedged() {
        // streaming is "long", but no ping for >2*ping_ms means the runtime is wedged.
        assert!(watchdog_breach(&wd(), Phase::Streaming, 10_000, Some(31_000), false).is_some());
    }

    #[test]
    fn streaming_with_fresh_pings_is_not_a_breach() {
        assert!(watchdog_breach(&wd(), Phase::Streaming, 120_000, Some(2_000), false).is_none());
    }

    #[test]
    fn await_feedback_with_tool_in_flight_is_not_a_breach() {
        assert!(watchdog_breach(&wd(), Phase::AwaitFeedback, 200_000, Some(2_000), true).is_none());
        // …same phase, NO tool in flight, past stall_cap → breach.
        assert!(watchdog_breach(&wd(), Phase::AwaitFeedback, 95_000, Some(2_000), false).is_some());
    }

    #[test]
    fn no_ping_yet_disarms_both_caps() {
        // Before the first ping (None), neither cap fires (cold-start grace).
        assert!(watchdog_breach(&wd(), Phase::AwaitingHeaders, 95_000, None, false).is_none());
    }

    #[test]
    fn fresh_pings_do_not_save_a_capped_fast_phase() {
        let cfg = wd();
        // ping just arrived (since=0) but awaiting_headers elapsed crossed stall_cap.
        assert!(watchdog_breach(&cfg, Phase::AwaitingHeaders, 90_001, Some(0), false).is_some());
    }

    // ── emit_terminal_failure ordering test ──────────────────────────────────

    #[test]
    fn terminal_failure_writes_error_event_and_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path();
        let session = "sterm";
        let sdir = gait_dir.join("sessions").join(session);
        std::fs::create_dir_all(&sdir).unwrap();
        let mut failed = false;
        let session_log = crate::turn::SessionLog::open(&sdir).unwrap();
        super::emit_terminal_failure(
            &session_log,
            &super::DiagnosticSink::new(gait_dir, session),
            &mut failed,
            crate::diagnostic::Phase::AwaitingHeaders,
            "watchdog: phase exceeded stall_cap",
        );

        assert!(failed, "brain_failed flag set for exit parity");
        let log_path = sdir.join("log.jsonl");
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.to_lowercase().contains("error"),
            "Error event in log.jsonl"
        );
        let diag = std::fs::read_to_string(sdir.join("diagnostic.jsonl")).unwrap();
        assert!(diag.contains("awaiting_headers") && diag.contains("stall_cap"));
    }
}
