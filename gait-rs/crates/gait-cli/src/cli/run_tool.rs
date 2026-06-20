//! Hidden `gait __run-tool` — the ephemeral "hand".
//!
//! Reads one binary `Impulse` on stdin, runs one built-in tool against a buffering
//! `Recorder` (under NoopSandbox semantics — the tool's own access checks are what
//! actually gates reads/writes/network), writes a binary `Feedback` on stdout, exits.
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
use gait_core::proto::Recorder;
use gait_core::tools::{builtin_tools, ToolCtx};
use gait_proto::gait::sandbox::v1::{Feedback, Impulse};

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
    let args_json: serde_json::Value = impulse
        .args
        .map(|s| serde_json::to_value(&s).unwrap_or(serde_json::Value::Object(Default::default())))
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
    let result_json = match tool.execute(args_json, &rec).await {
        Ok(v) => v,
        Err(e) => serde_json::Value::String(format!("error: {e}")),
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
    };

    let bytes = feedback.encode_to_vec();
    if let Err(e) = std::io::Write::write_all(&mut std::io::stdout(), &bytes) {
        eprintln!("[hand] stdout write failed: {e}");
        std::process::exit(1);
    }
}
