//! Hidden `gait __run-tool` — the ephemeral "hand".
//!
//! Reads one binary `Impulse` on stdin, applies the per-call Seatbelt/Landlock
//! seal based on the `Impulse.requested` scope, then runs the requested tool
//! against a buffering `Recorder`, writes a binary `Feedback` on stdout, exits.
//!
//! This is the Phase 2a subprocess boundary between the brain (turn loop) and a sealed
//! hand. It is intentionally hidden from `usage()` — there is no user-facing reason
//! to invoke it directly; the brain will spawn this binary from `turn.rs` (Phase 2b).
//!
//! Wire encoding is binary protobuf (prost `encode_to_vec` / `decode`), per the
//! [LOCKED] review correction. The plain-text `tool:read`/`args:...` form lives only
//! inside the `Impulse` and `Feedback` proto messages.
use std::io::Read;
use std::path::PathBuf;

use clap::Parser;
use prost::Message;

use gait_core::git::Repos;
use gait_core::proto::gait::log::v1::UnlockItem;
use gait_core::proto::Recorder;
use gait_core::sandbox::{NetworkPolicy, SandboxScope};
use gait_core::tools::{builtin_tools, ToolCtx};
use gait_proto::gait::sandbox::v1::{network_scope::Mode, Feedback, Impulse};

#[derive(Parser)]
#[command(no_binary_name = true)]
struct RunToolArgs {
    #[arg(long)]
    gait_dir: PathBuf,
    #[arg(long)]
    codebase: PathBuf,
    #[arg(long)]
    session: String,
    #[arg(long)]
    tool: String,
    /// When false, skip platform_sandbox().apply() and use NoopSandbox instead.
    /// Set by SubprocessSynapse based on the per-turn sandbox engagement flag.
    #[arg(long, default_value = "true")]
    seal: bool,
}

pub async fn cmd_run_tool(argv: &[String]) {
    let a = RunToolArgs::parse_from(argv);

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
    // pbjson_types::Struct serializes as a plain JSON object via its serde impl,
    // so a round-trip through serde_json is the cleanest portable conversion.
    // Borrow via `as_ref` so the `impulse` value remains whole for the seal build below.
    let args_json: serde_json::Value = impulse
        .args
        .as_ref()
        .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Object(Default::default())))
        .unwrap_or(serde_json::Value::Object(Default::default()));

    // For `shell`, inject impulse.cwd into args as `__cwd` so the tool reads the
    // session's active working directory without re-reading the session log itself.
    let args_json = if a.tool == "shell" {
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

    let ctx = ToolCtx::new(a.codebase.clone(), a.gait_dir.clone(), a.session.clone());
    let tools = builtin_tools(ctx, vec![], vec![], vec![], Repos::production());
    let tool = match tools.iter().find(|t| t.name() == a.tool) {
        Some(t) => t,
        None => {
            eprintln!("[hand] unknown tool: {}", a.tool);
            std::process::exit(1);
        }
    };

    let rec = Recorder::buffering();

    // Build the SandboxScope from Impulse.requested and apply the seal BEFORE any
    // tool I/O. Phase 2b: the hand enforces the kernel sandbox itself — this is
    // irreversible, so it must happen before tool.execute().
    let seal_scope = build_scope_from_impulse(&impulse);
    let seal_status = if a.seal {
        gait_core::sandbox::platform_sandbox()
            .apply(&seal_scope)
            .unwrap_or(gait_core::sandbox::SandboxStatus::Unavailable {
                reason: "sandbox apply error".into(),
            })
    } else {
        gait_core::sandbox::SandboxStatus::Noop
    };

    // Emit the SandboxApplied event so the brain can log it. Tools will see the
    // event appear alongside their own events in the feedback stream.
    let scope_paths: Vec<String> = seal_scope
        .writable
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let (enforcement_str, reason) = match &seal_status {
        gait_core::sandbox::SandboxStatus::Active => ("active", None),
        gait_core::sandbox::SandboxStatus::Unavailable { reason } => {
            ("unavailable", Some(reason.clone()))
        }
        gait_core::sandbox::SandboxStatus::Noop => ("noop", None),
    };
    rec.record(gait_core::proto::events::sandbox_applied(
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
    // commands. Scan the combined output (stdout+stderr, stored in the `output`
    // field of the shell's JSON result) for POSIX error strings that indicate
    // EACCES or EPERM. This is best-effort — false positives are low-harm in
    // supervised mode (the user sees an unlock prompt they can decline).
    // In auto mode the turn loop ignores `denied` and lets the tool failure
    // propagate normally, so false positives there have zero effect.
    let denied: Option<UnlockItem> = if a.tool == "shell" {
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
    // pbjson_types::Value implements serde Deserialize, so we round-trip via JSON.
    // Any shape that doesn't deserialize cleanly collapses to a null Value — the
    // brain then sees null from a tool call that actually succeeded. Log so that
    // the fallback is detectable.
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

/// Build a `SandboxScope` from `Impulse.requested` for the seal. The brain computes
/// the ceiling and ships it over the wire; the hand is responsible for turning it
/// into the kernel-level scope (Seatbelt profile on macOS, Landlock on Linux) and
/// applying it. Defaults to a Deny-network empty scope if the Impulse carries no
/// request (defensive — should not happen in practice).
fn build_scope_from_impulse(impulse: &Impulse) -> SandboxScope {
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

    // Phase 2d: brain computes read_all (from cfg.readable.contains("*")) and ships it
    // in Impulse. Honor whatever the brain sends: with read_all=false + empty readable,
    // platform defaults (dyld, /usr, /private, /etc, /dev, /System, /Library) + metadata
    // still apply, sufficient for tools like webfetch that only exec + TLS/DNS.
    let read_all = s.read_all;

    SandboxScope {
        readable: s.readable.iter().map(PathBuf::from).collect(),
        writable: s.writable.iter().map(PathBuf::from).collect(),
        read_all,
        network,
    }
}
