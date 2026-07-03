//! Spine-side helpers shared by the CLI spine (`gait run`) and the REPL spine
//! (`gait chat`).  Both spawn a `gait __brain` per turn and relay its Nerve
//! signals; this module owns that shared relay loop and the tool-execution helper.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::SandboxConfig;
use crate::port::output::TurnSink;
use crate::proto::gait::sandbox::v1::{signal, Notify};
#[allow(unused_imports)] // used in test-only wire::append calls
use crate::proto::wire;
use crate::turn::nerve;
use crate::turn::DiagnosticSink;

mod watchdog;
#[allow(unused_imports)] // Watchdog adopted by relay.rs in a later stage
pub(crate) use watchdog::{watchdog_breach, SpinePhase, Watchdog, WatchdogCfg};

mod preview;
pub(crate) use preview::enrich_permissions_preview;
mod util;
pub use util::strip_task_from_args;
pub(crate) use util::turn_result_from_json;

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
    /// Ephemeral one-off: brain registers only `structured_done` (no hands).
    pub hands_free: bool,
}

/// Selects whether a turn persists to the gait-dir. `Persistent` is every
/// production/test caller; `Ephemeral` is the `gait -e` one-off, which serves the
/// brain's input from memory and writes nothing to disk.
pub enum TurnMode {
    Persistent,
    Ephemeral {
        input: Vec<crate::proto::gait::log::v1::Event>,
    },
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
    let mut profile = crate::profile::Profile::from_config_and_agent(
        &cfg,
        gait_dir,
        Some(codebase),
        agent_path.as_deref(),
        inputs.model_override.clone(),
    );
    profile.hands_free = inputs.hands_free;
    profile
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
    crate::launch::BrainLaunch {
        gait_dir: gait_dir.to_path_buf(),
        codebase: codebase.to_path_buf(),
        session_id: session_id.to_string(),
        auto,
        max_iter,
        sandbox_on,
        allow: profile.allow.clone(),
        deny: profile.deny.clone(),
    }
    .to_argv()
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
    mode: &TurnMode,
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

    // Session-scoped git handle — holds only traces+checkpoints Arcs (no codebase
    // repo), so codebase mixing is structurally impossible.
    // Session-scoped git handle — null in ephemeral mode (no worktree/checkpoint/commit).
    let git = match mode {
        TurnMode::Persistent => crate::turn::SessionGit::for_session(repos, gait_dir, session_id),
        TurnMode::Ephemeral { .. } => crate::turn::SessionGit::null(),
    };

    let session_dir = gait_dir.join("sessions").join(session_id);

    // Persistent: worktree + checkpoint init + lock + SessionStart, exactly as before.
    // Ephemeral: an in-memory log seeded with the one-off input; no FS writes at all.
    let log = match mode {
        TurnMode::Persistent => {
            git.ensure_worktree()?;
            git.checkpoint_init(codebase)?;
            let log = crate::turn::SessionLog::open(&session_dir)?;
            // SessionStart — write once per session (idempotent via event check).
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
                    let tx_path = log
                        .log_path()
                        .expect("persistent log has a path")
                        .to_path_buf();
                    crate::session::log::init_session(crate::session::log::InitSessionOpts {
                        log_path: tx_path,
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
            log
        }
        TurnMode::Ephemeral { input } => crate::turn::SessionLog::ephemeral(input.clone()),
    };
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

    // ── BrainInit: push config + full log + git so the brain reads zero disk. ──
    // The caller appended this turn's user input before relay_brain_turn, and the
    // SessionStart write above ran, so `events` is complete.
    {
        use crate::proto::gait::sandbox::v1::{signal, BrainInit, DaemonStatus, GitStatus, Signal};
        use tokio::io::AsyncWriteExt;
        let cfg = crate::config::load_config(gait_dir);
        let config_json = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());
        let events = log.events();
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
    // Track in-flight ToolCall call_ids (→ tool names) so we can synthesize a
    // ToolResult for any that are orphaned if the brain is killed.
    let mut pending_calls: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut inflight: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    // Transitions: BrainPhase→HandsPhase on first ToolCall insert; back on drain.
    let mut spine_phase = SpinePhase::BrainPhase;

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
                if let Some(reason) =
                    watchdog_breach(&wd, last_phase, last_phase_elapsed_ms, since, spine_phase)
                {
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
                if impulse.tool == "spawn_agent" {
                    let ctx = ctx.clone();
                    inflight.spawn(async move {
                        let args = handlers::agent::parse_spawn_args(&impulse);
                        let feedback = handlers::agent::handle_spawn_agent(args, ctx).await;
                        handlers::reply_feedback(stdin_writer, corr_id, feedback).await;
                    });
                } else {
                    let cfg = sandbox_cfg.clone();
                    let gd = gait_dir.to_path_buf();
                    let cb = codebase.to_path_buf();
                    let sid = session_id.to_string();
                    let exe_c = exe.to_path_buf();
                    let grp = Arc::clone(&group);
                    inflight.spawn(async move {
                        let feedback = run_tool_as_spine(
                            impulse, &cfg, &gd, &cb, &sid, sandbox_on, &exe_c, grp,
                        )
                        .await;
                        handlers::reply_feedback(stdin_writer, corr_id, feedback).await;
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
                                pending_calls.insert(tc.call_id.clone(), tc.name.clone());
                                if pending_calls.len() == 1 {
                                    spine_phase = SpinePhase::HandsPhase;
                                }
                            }
                            Some(payload::Kind::ToolResult(tr)) => {
                                pending_calls.remove(&tr.call_id);
                                if pending_calls.is_empty() {
                                    spine_phase = SpinePhase::BrainPhase;
                                }
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
                // Route subagent completion through notify_parent (one-shot lock acquire,
                // dedup, write, commit, spawn_wake). Replaces the 20-retry open_retrying
                // path. ParentBusy → the live turn's per-step poll surfaces it next step.
                let c = crate::session::completion::Completion::Agent {
                    from: from_session.clone(),
                    status: status.clone(),
                    summary: summary.clone(),
                };
                match crate::session::completion::notify_parent(
                    gait_dir,
                    repos,
                    &target_session,
                    &c,
                ) {
                    crate::session::completion::NotifyOutcome::Woke => {}
                    crate::session::completion::NotifyOutcome::ParentBusy => {}
                    crate::session::completion::NotifyOutcome::AlreadySurfaced => {}
                    crate::session::completion::NotifyOutcome::NoParent => {
                        eprintln!(
                            "[spine] warning: parent session {target_session} not found for notify"
                        );
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
        if let Some(reason) =
            watchdog_breach(&wd, last_phase, last_phase_elapsed_ms, since, spine_phase)
        {
            emit_terminal_failure(&log, &diag, &mut brain_failed, last_phase, reason);
            let _ = group.kill_all();
            break;
        }
    }
    sink.end_turn();

    // Bound how long an in-flight tool call (or spawn_agent handler) can keep
    // running after the read loop has already decided the
    // turn is over. Must run before reap_brain: this is the spine's own
    // in-process task leak, distinct from (and not covered by) reap_brain's
    // OS-process-group kill.
    drain_inflight(inflight, std::time::Duration::from_secs(3)).await;

    // Synthesize a ToolResult for each orphaned ToolCall (brain was killed before
    // it could emit results). Tools that completed already have real results in the
    // log; only call_ids still in pending_calls are orphans. Must run before
    // commit_session so the repair lands in the same commit as the transcript.
    for (call_id, tool_name) in pending_calls.drain() {
        let msg = format!("[LLM timed out before '{}' result was received]", tool_name);
        let body = crate::proto::events::tool_result(call_id, msg, None, None);
        let prev = log.last_event();
        let ev = crate::proto::envelope::mint(body, prev.as_ref(), 0, 0);
        log.relay_event(&ev).ok();
    }

    // Drop the brain's stdin writer to signal EOF — the brain's Nerve reader
    // task is blocking on read_signal from stdin and will exit on EOF, which
    // lets the brain's tokio runtime shut down and the process exit.
    drop(child_stdin);

    // Single authoritative reap for EVERY exit path (EOF / Terminated / watchdog).
    // Reaps via kill_all() + Child drop — never tokio's child.wait(), which is
    // fatal on macOS (see `reap_brain`).
    reap_brain(Arc::clone(&group), child).await;

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
        let events = log.events();
        let turn_n = crate::session::log::count_turn_complete(&events) as u32;
        let _ = git.checkpoint(codebase, turn_n);

        // Commit the transcript to the session worktree.
        let _ = git.commit(&msg);
    }

    if let Some(result) = run_result {
        sink.result(&result);
    }

    Ok(brain_failed)
}

/// Reap the brain subprocess at end-of-turn, then drop the tokio `Child` without
/// ever awaiting it.
///
/// We deliberately do NOT call `child.wait().await`. On macOS, tokio registers a
/// kqueue `EVFILT_PROC`/`NOTE_EXIT` filter lazily on the first poll of `wait()`,
/// and registering that filter against a pid that has *already* exited never
/// delivers an event — the await blocks **forever**. Every end-of-turn path
/// reaches here with the brain already gone (its stdout pipe closed → EOF) or
/// just signalled to die, so this was a live hang: the spine would sit alive at
/// 0% CPU indefinitely, never running the post-turn checkpoint/commit.
///
/// `group.kill_all()` is the correct reaper and is idempotent (a no-op if the
/// watchdog already reaped in-loop). Platform specifics:
/// - macOS: `kill_all` blocking-`waitpid`s the brain — correct for an
///   already-exited zombie. If the brain is still alive it blocks until a detached
///   SIGTERM→(300ms)→SIGKILL escalation thread makes it exit. `spawn_blocking`
///   keeps that potential block off the runtime workers.
/// - Linux: `kill_all` reaps with `WNOHANG`, so a still-live brain is reaped by
///   tokio's orphan reaper when `child` is dropped (the macOS kqueue defect that
///   motivates this whole helper does not apply on Linux).
///
/// Either way we move `child` in and drop it WITHOUT awaiting. Reaping before the
/// drop means tokio's SIGCHLD reaper can't race-steal the pid; the post-drop
/// `try_wait` then sees `ECHILD` (harmless), so there is no zombie leak.
/// Give in-flight Impulse-handler tasks (spawn_agent and every other tool
/// exec — see the `tokio::spawn` sites in `relay_brain_turn`) up to `grace`
/// to finish naturally, then abort whatever's still running. Called once,
/// right after the read loop exits (EOF / Terminated / watchdog), and BEFORE
/// `reap_brain`: without this, a tool call that's still mid-flight when the
/// loop breaks can keep running — spawning subagent processes and writing
/// to the gait-dir — after the caller has already been told the turn is done.
///
/// `abort_all` cancels the *task* at its next await point but does not itself
/// kill any OS process a task was waiting on (e.g. `run_tool_as_spine`'s hand
/// subprocess) — it is enrolled in the cascade `group` when spawned
/// (`configure_enrolled`/`register_pid` in
/// `run_tool_as_spine`), so the `reap_brain` → `group.kill_all()` call that
/// MUST run immediately after this one is what actually kills them. Calling
/// this after `reap_brain`, or not at all, is the bug this function exists to
/// fix — do not reorder.
async fn drain_inflight(mut set: tokio::task::JoinSet<()>, grace: std::time::Duration) {
    let _ = tokio::time::timeout(grace, async { while set.join_next().await.is_some() {} }).await;
    set.abort_all();
    while set.join_next().await.is_some() {}
}

async fn reap_brain(group: Arc<dyn crate::process::Containment>, child: tokio::process::Child) {
    let _ = tokio::task::spawn_blocking(move || group.kill_all()).await;
    drop(child);
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
    // Always use ErrorPhase::Llm and a descriptive prefix so the transcript
    // clearly names the timeout rather than mapping a diagnostic phase string.
    let body = crate::proto::events::error(format!("LLM timed out: {detail}"), "llm");
    let prev = log.last_event();
    let evt = crate::proto::envelope::mint(body, prev.as_ref(), 0, 0);
    log.relay_event(&evt).ok();
    *brain_failed = true;

    // 2) DiagRecord → diagnostic.jsonl SECOND (always, even when GAIT_DEBUG is
    //    off — a terminal failure must always be explained).
    let rec = crate::diagnostic::DiagRecord::terminal(diag.session_id(), phase, detail);
    diag.terminal(&rec.to_json_line());
}

mod handlers;
pub(crate) use handlers::tool::run_tool_as_spine;
#[allow(unused_imports)] // path-stable facade re-export; no in-crate caller yet
pub(crate) use handlers::{error_feedback, TurnCtx};

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{build_turn_profile, drain_inflight, reap_brain, ProfileInputs};

    /// Regression for the spine end-of-turn hang: `reap_brain` must return promptly
    /// even when the brain has already exited. The old teardown called
    /// `child.wait().await` here, which on macOS hangs forever for an already-exited
    /// pid (kqueue `EVFILT_PROC` never fires) — the spine sat alive at 0% CPU and
    /// never ran the post-turn checkpoint/commit. The fix reaps via
    #[tokio::test]
    async fn drain_inflight_lets_fast_tasks_finish_normally() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let mut set = tokio::task::JoinSet::new();
        let finished = Arc::new(AtomicBool::new(false));
        let f = finished.clone();
        set.spawn(async move {
            f.store(true, Ordering::SeqCst);
        });
        drain_inflight(set, std::time::Duration::from_millis(200)).await;
        assert!(
            finished.load(Ordering::SeqCst),
            "fast task should have completed, not been aborted"
        );
    }

    #[tokio::test]
    async fn drain_inflight_aborts_tasks_that_outlive_the_grace_period() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let mut set = tokio::task::JoinSet::new();
        let reached_end = Arc::new(AtomicBool::new(false));
        let r = reached_end.clone();
        set.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            r.store(true, Ordering::SeqCst);
        });
        let start = std::time::Instant::now();
        drain_inflight(set, std::time::Duration::from_millis(50)).await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "drain_inflight should abort stragglers promptly, took {:?}",
            start.elapsed()
        );
        assert!(
            !reached_end.load(Ordering::SeqCst),
            "long task should have been aborted before completing"
        );
    }

    /// `group.kill_all()` (blocking waitpid) and never awaits the tokio `Child`.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn reap_brain_does_not_hang_on_already_exited_child() {
        // Build the child exactly as the spine does: std::Command → configure_enrolled
        // → tokio Command, so it joins a real cascade group the way the brain does.
        let group = crate::process::establish(None);
        let mut std_cmd = std::process::Command::new("true"); // exits 0 immediately
        group
            .configure_enrolled(&mut std_cmd)
            .expect("configure enrolled");
        let mut tokio_cmd = tokio::process::Command::from(std_cmd);
        let child = tokio_cmd.spawn().expect("spawn child");
        let pid = child.id().expect("pid") as i32;
        group.register_pid(pid);

        // Let it fully exit → it is now a zombie / already-reaped pid, the exact
        // condition that wedged `child.wait().await`.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        // Must complete well within the bound. On the old code this would block
        // indefinitely on macOS and the timeout would trip.
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reap_brain(group.clone(), child),
        )
        .await;
        assert!(res.is_ok(), "reap_brain hung on an already-exited child");

        // Brain is reaped — no zombie leak (second waitpid → ECHILD).
        let r = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(r, -1, "brain pid should already be reaped (ECHILD)");
    }

    // `brain_argv_is_canonical_and_has_no_role_token` and
    // `brain_argv_unsandboxed_emits_no_sandbox` moved to
    // `launch::tests` (spec-00-footguns.md item 4) — they now assert
    // against `BrainLaunch::to_argv()` directly, plus a round-trip parse
    // through `BrainArgs` that these two didn't have before.

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
            hands_free: false,
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
            hands_free: false,
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
            hands_free: false,
        };
        // Should panic: no model in config and no model_override supplied.
        let _ = build_turn_profile(dir.path(), dir.path(), &inputs);
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
        // tool_name "read" is the value stored in pending_calls (call_id → tool_name).
        let body = crate::proto::events::tool_result(
            call_id.clone(),
            "[LLM timed out before 'read' result was received]".into(),
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
                    assert!(
                        tr.output.contains("LLM timed out") && tr.output.contains("read"),
                        "output must name the tool and the cause; got: {}",
                        tr.output
                    );
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
            crate::diagnostic::Phase::Streaming,
            "absolute backstop exceeded",
        );

        assert!(failed, "brain_failed flag set for exit parity");
        let log_path = sdir.join("log.jsonl");
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.to_lowercase().contains("error"),
            "Error event in log.jsonl"
        );
        let diag = std::fs::read_to_string(sdir.join("diagnostic.jsonl")).unwrap();
        assert!(diag.contains("streaming") && diag.contains("absolute backstop"));
    }
}
