//! Hand runtime: the sealed, per-tool-call executor process.
//!
//! The hand is spawned by the spine (`gait __run-tool`), reads one binary
//! `Impulse` on stdin, applies the per-call Seatbelt/Landlock seal based on
//! the `Impulse.requested` scope, runs the requested tool against a buffering
//! `Recorder`, writes a binary `Feedback` on stdout, exits.
//!
//! This is the Phase 2a subprocess boundary between the brain (turn loop) and
//! a sealed hand. Wire encoding is binary protobuf (prost `encode_to_vec` /
//! `decode`).

use std::io::Read;
use std::path::PathBuf;

use prost::Message as _;

use crate::git::Repos;
use crate::proto::gait::log::v1::UnlockItem;
use crate::proto::Recorder;
use crate::sandbox::{NetworkPolicy, SandboxScope};
use crate::tools::{builtin_tools, ToolCtx};
use gait_proto::gait::sandbox::v1::{
    hand_response, network_scope::Mode, DaemonRecordProto, Feedback, HandError, HandResponse,
    Impulse,
};

// ── respond helpers ───────────────────────────────────────────────────────────

/// Encode one `HandResponse` to stdout and exit. Every hand outcome goes
/// through here — there is no other reply channel.
fn respond(outcome: hand_response::Outcome) -> ! {
    let resp = HandResponse {
        outcome: Some(outcome),
    };
    let bytes = resp.encode_to_vec();
    if let Err(e) = std::io::Write::write_all(&mut std::io::stdout(), &bytes) {
        // stdout is gone — nothing more we can do; the spine's crash fallback handles it.
        #[allow(clippy::print_stderr)]
        // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
        {
            eprintln!("[hand] stdout write failed: {e}");
        }
        std::process::exit(1);
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::process::exit(0);
}

fn respond_err(msg: impl Into<String>) -> ! {
    respond(hand_response::Outcome::Error(HandError {
        message: msg.into(),
    }))
}

// ── sigterm guard ─────────────────────────────────────────────────────────────

/// On a teardown SIGTERM (sent by the spine's `kill_all` before its 300ms→SIGKILL
/// escalation), SIGKILL the in-flight command's subtree so an own-process-group
/// command isn't orphaned, then exit. No-op-ish if no command is running.
#[cfg(unix)]
fn install_sigterm_guard() {
    use tokio::signal::unix::{signal, SignalKind};
    if let Ok(mut term) = signal(SignalKind::terminate()) {
        tokio::spawn(async move {
            term.recv().await;
            crate::process::scope::kill_active();
            std::process::exit(143);
        });
    }
}

// ── HandConfig ────────────────────────────────────────────────────────────────

/// Configuration extracted from CLI args, passed to `run_hand`.
/// Defined in core so `run_hand` doesn't depend on clap or gait-cli.
#[derive(Debug, Clone)]
pub struct HandConfig {
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub session: String,
    pub tool: String,
    /// When false, skip platform_sandbox().apply() and use NoopSandbox instead.
    /// Set by the spine based on the per-turn sandbox engagement flag.
    pub seal: bool,
}

// ── run_hand ──────────────────────────────────────────────────────────────────

/// The hand's entry point. Called by `cmd_run_tool` in gait-cli after arg parsing.
///
/// 1. Read binary `Impulse` from stdin.
/// 2. Build `SandboxScope` from `Impulse.requested` and apply the seal.
/// 3. Run the requested tool against a buffering `Recorder`.
/// 4. Write binary `Feedback` on stdout.
pub async fn run_hand(config: HandConfig) {
    #[cfg(unix)]
    install_sigterm_guard();

    // Read binary Impulse from stdin.
    let mut buf = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
        respond_err(format!("stdin read failed: {e}"));
    }
    let impulse = match Impulse::decode(buf.as_slice()) {
        Ok(r) => r,
        Err(e) => respond_err(format!("bad Impulse: {e}")),
    };

    // Convert Impulse.args (Option<pbjson_types::Struct>) → serde_json::Value.
    // pbjson_types stores all numbers as f64, so integer fields come out as floats
    // (e.g. `limit: 3.0`). Coerce whole-number floats back to integers so serde
    // can deserialize them into u64/i64 fields without error.
    let args_json: serde_json::Value = {
        let raw = impulse
            .args
            .as_ref()
            .map(|s| {
                serde_json::to_value(s).unwrap_or(serde_json::Value::Object(Default::default()))
            })
            .unwrap_or(serde_json::Value::Object(Default::default()));
        crate::tools::coerce_int_params(raw)
    };

    // For `shell`, inject impulse.cwd into args as `__cwd` so the tool reads the
    // session's active working directory without re-reading the session log itself.
    let args_json = if config.tool == "shell" {
        let mut obj = match args_json {
            serde_json::Value::Object(m) => m,
            other => {
                let mut m = serde_json::Map::new();
                if !other.is_null() {
                    m.insert("_raw".into(), other);
                }
                m
            }
        };
        obj.entry("__cwd")
            .or_insert_with(|| serde_json::Value::String(impulse.cwd.clone()));
        serde_json::Value::Object(obj)
    } else {
        args_json
    };

    let ctx = {
        let mut ctx = ToolCtx::new(
            config.codebase.clone(),
            config.gait_dir.clone(),
            config.session.clone(),
        );
        // Extract the network allowlist from the Impulse scope so webfetch
        // can enforce host-level filtering (the kernel SBPL allows all outbound
        // when Hosts mode is set — the app-level check closes the gap).
        if let Some(s) = impulse.requested.as_ref() {
            if let Some(n) = s.network.as_ref() {
                if let Ok(gait_proto::gait::sandbox::v1::network_scope::Mode::Hosts) =
                    gait_proto::gait::sandbox::v1::network_scope::Mode::try_from(n.mode)
                {
                    ctx = ctx.with_network_allowlist(n.hosts.clone());
                }
            }
        }
        ctx
    };
    let tools = builtin_tools(ctx, vec![], vec![], vec![], Repos::production());
    let tool = match tools.iter().find(|t| t.name() == config.tool) {
        Some(t) => t,
        None => respond_err(format!("unknown tool: {}", config.tool)),
    };

    // Schema-guided coercion (string -> scalar, float -> int) needs the tool's
    // parameters schema, so it lives here, after the tool is resolved. The
    // injected `__cwd` key has no schema entry and falls through to the float->int
    // pass (a string path is left untouched).
    let args_json = crate::tools::coerce_scalar_params(args_json, tool.parameters());

    let rec = Recorder::buffering();

    // Build the SandboxScope from Impulse.requested and apply the seal BEFORE any
    // tool I/O. The hand enforces the kernel sandbox itself — this is irreversible.
    let seal_scope = build_scope_from_impulse(&impulse);
    let seal_status = if config.seal {
        crate::sandbox::platform_sandbox()
            .apply(&seal_scope)
            .unwrap_or(crate::sandbox::SandboxStatus::Unavailable {
                reason: "sandbox apply error".into(),
            })
    } else {
        crate::sandbox::SandboxStatus::Noop
    };

    // Emit the SandboxApplied event so the brain can log it.
    let scope_paths: Vec<String> = seal_scope
        .writable
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let (enforcement_str, reason) = match &seal_status {
        crate::sandbox::SandboxStatus::Active => ("active", None),
        crate::sandbox::SandboxStatus::Unavailable { reason } => {
            ("unavailable", Some(reason.clone()))
        }
        crate::sandbox::SandboxStatus::Noop => ("noop", None),
    };
    rec.record(crate::proto::events::sandbox_applied(
        enforcement_str,
        scope_paths,
        reason,
    ))
    .ok();

    let mut result_json = match tool.execute(args_json, &rec).await {
        Ok(v) => v,
        Err(e) => serde_json::Value::String(format!("error: {e}")),
    };

    // Extract "__daemon" sideband if the shell tool spawned a detached daemon.
    // The shell tool embeds the DaemonRecord fields under this key so it can
    // travel through the execute() return value without a separate channel.
    let daemon_record: Option<DaemonRecordProto> = if let Some(obj) = result_json.as_object_mut() {
        if let Some(d) = obj.remove("__daemon") {
            let name = d
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let pid = d.get("pid").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let pgid = d.get("pgid").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let started_at = d.get("started_at").and_then(|v| v.as_u64()).unwrap_or(0);
            let cmd = d
                .get("cmd")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let cwd = d
                .get("cwd")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let log_path = d
                .get("log_path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(DaemonRecordProto {
                name,
                pid,
                pgid,
                started_at,
                cmd,
                cwd,
                log_path,
                // Owning session id (Task 6 — Review Correction [E]): the registry
                // record must remember which parent session to wake/notify on exit.
                session: config.session.clone(),
            })
        } else {
            None
        }
    } else {
        None
    };

    // Phase 2c Task 6: detect kernel-level permission denial from sealed `shell`
    // commands. Scan the combined output for POSIX error strings that indicate
    // EACCES or EPERM. Best-effort — false positives are low-harm in supervised
    // mode (the user sees an unlock prompt they can decline).
    let denied: Option<UnlockItem> = if config.tool == "shell" {
        let output_str = result_json
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let has_denial = output_str.contains("Permission denied")
            || output_str.contains("Operation not permitted");
        if has_denial {
            Some(UnlockItem {
                id: uuid::Uuid::new_v4().to_string(),
                tool_name: "shell".to_string(),
                resource: "(shell)".to_string(),
                dimension: "write".to_string(),
                preview: None,
            })
        } else {
            None
        }
    } else {
        None
    };

    // Convert serde_json::Value → pbjson_types::Value.
    let result_proto: pbjson_types::Value = serde_json::from_value(result_json.clone())
        .unwrap_or_else(|e| {
            #[allow(clippy::print_stderr)] // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
            {
                eprintln!(
                    "[hand] result→proto conversion failed ({e}); returning null. raw: {result_json}"
                );
            }
            pbjson_types::Value { kind: None }
        });

    let feedback = Feedback {
        result: Some(result_proto),
        events: rec.take_buffer(),
        denied,
        daemon: daemon_record,
    };
    respond(hand_response::Outcome::Feedback(feedback));
}

// ── build_scope_from_impulse ──────────────────────────────────────────────────

/// Build a `SandboxScope` from `Impulse.requested` for the seal. The brain
/// computes the ceiling and ships it over the wire; the hand is responsible for
/// turning it into the kernel-level scope (Seatbelt profile on macOS, Landlock
/// on Linux) and applying it. Defaults to a Deny-network empty scope if the
/// Impulse carries no request (defensive — should not happen in practice).
pub fn build_scope_from_impulse(impulse: &Impulse) -> SandboxScope {
    let Some(s) = impulse.requested.as_ref() else {
        return SandboxScope::default();
    };

    let network = s
        .network
        .as_ref()
        .map(|n| match Mode::try_from(n.mode).unwrap_or(Mode::Deny) {
            Mode::All => NetworkPolicy::All,
            Mode::Hosts => NetworkPolicy::Hosts(n.hosts.clone()),
            Mode::Deny => NetworkPolicy::Deny,
        })
        .unwrap_or(NetworkPolicy::Deny);

    // Phase 2d: brain computes read_all (from cfg.readable.contains("*")) and
    // ships it in Impulse. Honor whatever the brain sends.
    let read_all = s.read_all;

    SandboxScope {
        readable: s.readable.iter().map(PathBuf::from).collect(),
        writable: s.writable.iter().map(PathBuf::from).collect(),
        read_all,
        network,
    }
}
