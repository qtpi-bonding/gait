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
use gait_proto::gait::sandbox::v1::{network_scope::Mode, Feedback, Impulse};

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
    // Read binary Impulse from stdin.
    let mut buf = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
        eprintln!("[hand] stdin read failed: {e}");
        std::process::exit(1);
    }
    let impulse = match Impulse::decode(buf.as_slice()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[hand] bad Impulse: {e}");
            std::process::exit(1);
        }
    };

    // Convert Impulse.args (Option<pbjson_types::Struct>) → serde_json::Value.
    let args_json: serde_json::Value = impulse
        .args
        .as_ref()
        .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Object(Default::default())))
        .unwrap_or(serde_json::Value::Object(Default::default()));

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
        None => {
            eprintln!("[hand] unknown tool: {}", config.tool);
            std::process::exit(1);
        }
    };

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

    let result_json = match tool.execute(args_json, &rec).await {
        Ok(v) => v,
        Err(e) => serde_json::Value::String(format!("error: {e}")),
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
            eprintln!(
                "[hand] result→proto conversion failed ({e}); returning null. raw: {result_json}"
            );
            pbjson_types::Value { kind: None }
        });

    let feedback = Feedback {
        result: Some(result_proto),
        events: rec.take_buffer(),
        denied,
    };

    let bytes = feedback.encode_to_vec();
    if let Err(e) = std::io::Write::write_all(&mut std::io::stdout(), &bytes) {
        eprintln!("[hand] stdout write failed: {e}");
        std::process::exit(1);
    }
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
