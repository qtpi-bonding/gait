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
use std::path::{Path, PathBuf};

use prost::Message as _;

use crate::git::Repos;
use crate::proto::gait::log::v1::SandboxUnlockItem;
use crate::proto::Recorder;
use crate::sandbox::{DenyPolicy, NetworkPolicy, SandboxScope};
use crate::tools::fs_backend::{BufferedFs, FsBackend, LocalFs};
use crate::tools::{builtin_tools, ToolCtx};
use gait_proto::gait::sandbox::v1::{
    hand_response, network_scope::Mode, Feedback, HandError, HandResponse, Impulse,
    OutputCaptureMode as ProtoOutputCaptureMode, ProcessRecordProto,
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
        crate::trace::exit(1);
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());
    crate::trace::exit(0);
}

fn err_outcome(msg: impl Into<String>) -> hand_response::Outcome {
    hand_response::Outcome::Error(HandError {
        message: msg.into(),
    })
}

/// Failure message when a `~` deny entry cannot be expanded: the tool call
/// was not run and the hand fails closed.
fn deny_home_unresolved_message(
    err: &gait_sandbox::sandbox::ceiling::DenyHomeUnresolved,
) -> String {
    format!(
        "error[deny_home_unresolved]: {}. This tool call was not run. \
         Do not retry it; tell the user to set HOME to an absolute path.",
        err
    )
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
    pub output_spool: Option<PathBuf>,
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
    // The body finishes (closing the hand's span) before `respond` exits the
    // process, so the span is exported.
    // The hand's span and exporter start inside `hand_body`, after the seal.
    let outcome = hand_body(config).await;
    respond(outcome)
}

async fn hand_body(config: HandConfig) -> hand_response::Outcome {
    // Read binary Impulse from stdin.
    let mut buf = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
        return err_outcome(format!("stdin read failed: {e}"));
    }
    let impulse = match Impulse::decode(buf.as_slice()) {
        Ok(r) => r,
        Err(e) => return err_outcome(format!("bad Impulse: {e}")),
    };
    let declared_output_mode = match ProtoOutputCaptureMode::try_from(impulse.output_capture_mode) {
        Ok(ProtoOutputCaptureMode::Returned) => crate::types::OutputCaptureMode::Returned,
        Ok(ProtoOutputCaptureMode::Spool) => crate::types::OutputCaptureMode::Spool,
        Ok(ProtoOutputCaptureMode::Unspecified) | Err(_) => {
            return err_outcome("invalid or unspecified output capture mode")
        }
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

    // Computed before ToolCtx so tools (the shell tool in particular) can check
    // a requested `cwd` against scope before committing to it, instead of only
    // finding out when the kernel seal later rejects the subprocess spawn.
    // Pure/side-effect-free (only reads `impulse`) — safe to compute this early;
    // the seal itself still applies at the same point as before, below.
    let seal_scope = match build_scope_from_impulse(&impulse, &config.gait_dir, &config.codebase) {
        Ok(scope) => scope,
        Err(e) => return err_outcome(deny_home_unresolved_message(&e)),
    };

    let ctx = {
        let mut ctx = ToolCtx::new(
            config.codebase.clone(),
            config.gait_dir.clone(),
            config.session.clone(),
        )
        .with_scope(seal_scope.clone());
        if let Some(path) = config.output_spool.clone() {
            ctx = ctx.with_output_spool(path);
        }
        ctx.fs = select_fs(&impulse); // BufferedFs (delegated) or LocalFs (disk)
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
    let fs_handle = std::sync::Arc::clone(&ctx.fs);
    let supplied_output_mode = ctx.output_capture.mode();
    let tools = builtin_tools(ctx, vec![], vec![], vec![], Repos::production());
    let tool = match tools.iter().find(|t| t.name() == config.tool) {
        Some(t) => t,
        None => return err_outcome(format!("unknown tool: {}", config.tool)),
    };
    if tool.output_capture_mode() != declared_output_mode
        || declared_output_mode != supplied_output_mode
    {
        return err_outcome(format!(
            "output capture mismatch for {}: tool declares {:?}, impulse declares {:?}, hand received {:?}",
            config.tool,
            tool.output_capture_mode(),
            declared_output_mode,
            supplied_output_mode
        ));
    }

    // Schema-guided coercion (string -> scalar, float -> int) needs the tool's
    // parameters schema, so it lives here, after the tool is resolved.
    let args_json = crate::tools::coerce_scalar_params(args_json, tool.parameters());

    // App-level deny-policy gate -- check-before-serve, not react-after-fail.
    // It covers only the read, write and apply_patch tools (never `shell`), and
    // it matches path prefixes without resolving symlinks or `..`. It is the
    // app-level layer; on Linux the Landlock seal adds kernel-level path denies.
    // On macOS the Seatbelt profile also emits `deny` rules for deny subpaths
    // (sandbox.rs `build_sbpl`). An earlier kernel deny attempt was reverted
    // after a macOS sandbox-compiler quirk silently dropped trailing `deny`
    // rules; the current rules are tested through `sandbox-exec` only, NOT
    // through the in-process `sandbox_apply` path gait uses.
    let proactive_denial =
        classify_proactive_denial(&config.tool, &impulse, &config.codebase, &seal_scope);

    let rec = Recorder::buffering();

    // A detached shell writes its log under `<gait_dir>/processes/<session>`. Landlock
    // grants rules only to paths that exist when the seal is applied, so create the
    // directory first (a failure surfaces from the tool itself).
    if config.seal
        && config.tool == "shell"
        && args_json.get("detached").and_then(|v| v.as_bool()) == Some(true)
        && std::fs::create_dir_all(config.gait_dir.join("processes").join(&config.session)).is_err()
    {
        // The tool's own create_dir_all reports the failure.
    }

    // Apply the seal BEFORE any tool I/O. The hand enforces the kernel sandbox
    // itself — this is irreversible.
    let seal_status = if config.seal {
        let applied = crate::sandbox::platform_sandbox().apply(&seal_scope);
        match settle_seal(local_sandbox_require(&config.gait_dir), applied) {
            Ok(status) => status,
            Err(required) => return err_outcome(required),
        }
    } else {
        crate::sandbox::SandboxStatus::Noop
    };

    // Start telemetry only now: its exporter threads begin life under the seal
    // (a thread created before it would stay unconfined), so spans reach the
    // collector only if the seal's network policy allows it.
    crate::telemetry::init_deferred();
    let span = crate::trace::hand_span(&config.tool, &config.session);

    // Emit the SandboxApplied event so the brain can log it.
    let scope_paths: Vec<String> = seal_scope
        .writable
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let (enforcement_str, reason) = match &seal_status {
        crate::sandbox::SandboxStatus::Active => ("active", None),
        crate::sandbox::SandboxStatus::ActiveWithNotes { notes } => {
            ("active", Some(notes.join("; ")))
        }
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

    let mut result_json = if proactive_denial.is_some() {
        // Do NOT call tool.execute() — the whole point of check-before-serve is
        // that a denied path's content never reaches gait's own code, let alone
        // the model. No disk I/O happens for this call at all.
        serde_json::Value::String(
            "error: denied by sandbox deny policy (matches a configured secrets pattern)"
                .to_string(),
        )
    } else {
        match crate::trace::within(span.clone(), tool.execute(args_json, &rec)).await {
            Ok(v) => v,
            Err(e) => serde_json::Value::String(e.model_text(tool.name())),
        }
    };

    // Extract "__process" sideband if the shell tool spawned a detached process.
    // The shell tool embeds the ProcessRecord fields under this key so it can
    // travel through the execute() return value without a separate channel.
    let process_record: Option<ProcessRecordProto> = if let Some(obj) = result_json.as_object_mut()
    {
        if let Some(d) = obj.remove("__process") {
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
            Some(ProcessRecordProto {
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
    const FILESYSTEM_DENIAL_SIGNATURES: &[&str] = &["Permission denied", "Operation not permitted"];
    // Network-denial signatures gated on network_denied (impulse.requested's
    // NetworkScope.mode was DENY for this call) so a real network outage
    // isn't misattributed to the sandbox.
    const NETWORK_DENIAL_SIGNATURES: &[&str] = &[
        "Failed host lookup",
        "SocketException",
        "CERTIFICATE_VERIFY_FAILED",
        "Connection refused",
    ];

    let output_str = result_json
        .get("output")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let fs_signature = FILESYSTEM_DENIAL_SIGNATURES
        .iter()
        .find(|sig| output_str.contains(*sig));
    let denied: Option<SandboxUnlockItem> = if config.tool == "shell" && fs_signature.is_some() {
        Some(SandboxUnlockItem {
            id: uuid::Uuid::new_v4().to_string(),
            tool_name: "shell".to_string(),
            resource: "(shell)".to_string(),
            dimension: "write".to_string(),
            preview: None,
        })
    } else {
        None
    };

    let network_denied = impulse
        .requested
        .as_ref()
        .and_then(|s| s.network.as_ref())
        .map(|n| n.mode == gait_proto::gait::sandbox::v1::network_scope::Mode::Deny as i32)
        .unwrap_or(true);
    let shell_cmd = impulse
        .args
        .as_ref()
        .and_then(|s| s.fields.get("cmd"))
        .and_then(|v| match &v.kind {
            Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let network_signature = if network_denied {
        NETWORK_DENIAL_SIGNATURES
            .iter()
            .find(|sig| output_str.contains(*sig))
    } else {
        None
    };
    let sandbox_denied = if let Some(ref d) = proactive_denial {
        // read/write/apply_patch: this IS the deny policy firing (check-before-
        // serve, not react-after-fail) — matched_deny_pattern is always known
        // here, no error-text scanning needed.
        Some(crate::proto::gait::log::v1::SandboxDenied {
            tool_name: config.tool.clone(),
            resource: d.target.to_string_lossy().into_owned(),
            dimension: d.dimension.to_string(),
            matched_signature: String::new(),
            hint: None,
            matched_deny_pattern: Some(d.pattern.clone()),
        })
    } else if config.tool == "shell" {
        fs_signature
            .map(|sig| (sig, "write"))
            .or_else(|| network_signature.map(|sig| (sig, "network")))
            .map(|(sig, dimension)| {
                let hint = if dimension == "network" {
                    build_network_denial_hint(&shell_cmd, &impulse.shell_whitelist_patterns)
                } else {
                    None
                };
                crate::proto::gait::log::v1::SandboxDenied {
                    tool_name: "shell".to_string(),
                    resource: shell_cmd.clone(),
                    dimension: dimension.to_string(),
                    matched_signature: (*sig).to_string(),
                    hint,
                    // Best-effort path extraction from shell output is a
                    // documented gap (see classify_proactive_denial's doc
                    // comment) — shell commands aren't proactively gated.
                    matched_deny_pattern: None,
                }
            })
    } else {
        None
    };

    // Return the raw tool value. The spine owns the single output-finalization
    // boundary: it archives ordinary values or validates the preallocated file
    // populated by streaming tools, then builds the bounded model preview.
    // Convert serde_json::Value → pbjson_types::Value for that hand→spine hop.
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

    let final_content = fs_handle
        .captured_write()
        .map(|b| String::from_utf8_lossy(&b).into_owned());

    let feedback = Feedback {
        result: Some(result_proto),
        events: rec.take_buffer(),
        denied,
        process: process_record,
        final_content,
        whitelist_match_items: vec![],
        sandbox_denied,
        is_error: None,
    };
    hand_response::Outcome::Feedback(feedback)
}

// ── build_scope_from_impulse ──────────────────────────────────────────────────

/// Hard floor: read_deny/write_deny are ALWAYS sourced from the hand's own
/// locally-loaded SandboxConfig, never from `impulse.requested` — a brain
/// (sealed, network-facing, distrusted) must not be able to supply or widen
/// these two fields regardless of what it sends over the wire. This is what
/// makes the deny policy a floor rather than a claim.
fn local_deny_policy(
    gait_dir: &Path,
    codebase: &Path,
) -> Result<
    (crate::sandbox::DenyPolicy, crate::sandbox::DenyPolicy),
    gait_sandbox::sandbox::ceiling::DenyHomeUnresolved,
> {
    local_deny_policy_with(
        gait_dir,
        codebase,
        |k| std::env::var_os(k),
        || gait_sandbox::sandbox::ceiling::native_home().ok(),
    )
}

/// [`local_deny_policy`] with the environment supplied by `var`. The deny
/// entries are expanded through the same path-var expansion the sandbox uses
/// for its readable/writable entries (`~/` -> home, `$CODEBASE` -> codebase).
fn local_deny_policy_with(
    gait_dir: &Path,
    codebase: &Path,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    native_home: impl Fn() -> Option<PathBuf>,
) -> Result<
    (crate::sandbox::DenyPolicy, crate::sandbox::DenyPolicy),
    gait_sandbox::sandbox::ceiling::DenyHomeUnresolved,
> {
    let cfg = gait_types::config::load_config(gait_dir);
    let home = var("HOME")
        .and_then(|v| v.into_string().ok())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(native_home);
    let (mut read, mut write) = (
        gait_sandbox::sandbox::ceiling::split_deny_patterns_with_home(
            &cfg.sandbox.deny.read,
            home.as_deref(),
            codebase,
        )?,
        gait_sandbox::sandbox::ceiling::split_deny_patterns_with_home(
            &cfg.sandbox.deny.write,
            home.as_deref(),
            codebase,
        )?,
    );
    // The agent's identity folders: the device key must never reach the model.
    // The spine names them; the minted default beside gait_dir is always covered.
    let mut identity: Vec<PathBuf> =
        vec![gait_dir.join("identity"), gait_dir.join("identity-root")];
    if let Some(list) = var(gait_identity::DENY_PATHS_ENV) {
        identity.extend(std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()));
    }
    // The machine identity dir is denied by the hand's own floor too, whether or
    // not the spine named it; the spine fails closed when identity is in use,
    // while standalone gait just has no machine dir to deny.
    if let Ok(machine) = gait_identity::machine_dir_with(&var) {
        identity.push(machine);
    }
    for path in identity {
        read.subpaths.push(path.clone());
        write.subpaths.push(path);
    }
    Ok((read, write))
}

/// Whether the hand's own local config demands a working kernel seal. Like the
/// deny policy this is never taken from the Impulse.
fn local_sandbox_require(gait_dir: &Path) -> bool {
    gait_types::config::load_config(gait_dir).sandbox.require
}

/// The seal is required but unavailable: the tool call is not run.
#[derive(Debug, thiserror::Error)]
#[error(
    "error[sandbox_required]: the kernel sandbox is unavailable ({reason}) and \
     sandbox.require is set, so this tool call was not run. Do not retry it; tell \
     the user the sandbox must be enabled or sandbox.require turned off."
)]
struct SandboxRequired {
    reason: String,
}

impl From<SandboxRequired> for String {
    fn from(required: SandboxRequired) -> String {
        required.to_string()
    }
}

/// Settle the seal outcome against `require`: a seal that is `Unavailable`, or
/// whose apply returned `Err`, is fatal for the tool call when `require` is set.
fn settle_seal(
    require: bool,
    applied: anyhow::Result<crate::sandbox::SandboxStatus>,
) -> Result<crate::sandbox::SandboxStatus, SandboxRequired> {
    let status = applied.unwrap_or(crate::sandbox::SandboxStatus::Unavailable {
        reason: "sandbox apply error".into(),
    });
    match (&status, require) {
        (crate::sandbox::SandboxStatus::Unavailable { reason }, true) => Err(SandboxRequired {
            reason: reason.clone(),
        }),
        _ => Ok(status),
    }
}

/// One proactive deny-policy match: which dimension (read/write) matched, the
/// resolved target path, and which configured pattern matched it.
struct ProactiveDenial {
    dimension: &'static str,
    target: PathBuf,
    pattern: String,
}

/// Check-before-serve: for `read`/`write`/`apply_patch` tool calls, resolve the
/// target path (the same resolution the spine's FsDelegate prefetch already
/// uses — see `delegated_target`) and check it against the hard-floored
/// `read_deny`/`write_deny` on `scope` BEFORE the tool ever touches disk. This
/// is currently the ONLY thing enforcing gait's secrets-denylist feature — see
/// the doc comment at this function's call site in `run_hand` for why
/// (kernel-level SBPL `deny` rules were found unreliable on real hardware and
/// reverted; this app-level check is the actual enforcement, not a redundant
/// second layer over working kernel enforcement).
///
/// `shell` is deliberately NOT covered here — a shell command can touch
/// arbitrary paths gait can't enumerate without parsing shell syntax, which is
/// out of scope. This is a documented gap, not an oversight: `cat .env` run
/// via the `shell` tool is not blocked by this check.
fn classify_proactive_denial(
    tool: &str,
    impulse: &Impulse,
    codebase: &Path,
    scope: &SandboxScope,
) -> Option<ProactiveDenial> {
    let target = crate::turn::spine::delegated_target(tool, &impulse.args, codebase)?;
    match tool {
        "read" => {
            classify_denial_by_path(&target, &scope.read_deny).map(|pattern| ProactiveDenial {
                dimension: "read",
                target,
                pattern,
            })
        }
        "write" => {
            classify_denial_by_path(&target, &scope.write_deny).map(|pattern| ProactiveDenial {
                dimension: "write",
                target,
                pattern,
            })
        }
        "apply_patch" => {
            // apply_patch both reads existing content (to build the diff
            // context) and writes the result — check both dimensions,
            // whichever matches first.
            classify_denial_by_path(&target, &scope.read_deny)
                .map(|pattern| ProactiveDenial {
                    dimension: "read",
                    target: target.clone(),
                    pattern,
                })
                .or_else(|| {
                    classify_denial_by_path(&target, &scope.write_deny).map(|pattern| {
                        ProactiveDenial {
                            dimension: "write",
                            target,
                            pattern,
                        }
                    })
                })
        }
        _ => None,
    }
}

/// Compare a known target path against a deny policy, using the `glob` crate
/// for pattern matching. Returns the first matched pattern string, or `None`
/// if the path isn't denied by any pattern. This is the ONLY classification
/// mechanism available — the kernel doesn't report which rule fired even when
/// it does enforce something, so gait must do this comparison itself using
/// data it already has locally.
fn classify_denial_by_path(target: &Path, deny: &DenyPolicy) -> Option<String> {
    for subpath in &deny.subpaths {
        if target.starts_with(subpath) {
            return Some(subpath.to_string_lossy().into_owned());
        }
    }
    for pattern in &deny.globs {
        if let Ok(compiled) = glob::Pattern::new(pattern) {
            if compiled.matches_path(target) {
                return Some(pattern.clone());
            }
        }
    }
    None
}

/// Build a `SandboxScope` from `Impulse.requested` for the seal. The brain
/// computes the ceiling and ships it over the wire; the hand is responsible for
/// turning it into the kernel-level scope (Seatbelt profile on macOS, Landlock
/// on Linux) and applying it. Defaults to a Deny-network empty scope if the
/// Impulse carries no request (defensive — should not happen in practice).
/// `read_deny`/`write_deny` are ALWAYS sourced from `gait_dir`'s local config —
/// see [`local_deny_policy`] — never from `impulse.requested`, even in the
/// defensive empty-scope branch below.
pub fn build_scope_from_impulse(
    impulse: &Impulse,
    gait_dir: &Path,
    codebase: &Path,
) -> Result<SandboxScope, gait_sandbox::sandbox::ceiling::DenyHomeUnresolved> {
    let (read_deny, write_deny) = local_deny_policy(gait_dir, codebase)?;

    let Some(s) = impulse.requested.as_ref() else {
        return Ok(SandboxScope {
            read_deny,
            write_deny,
            ..SandboxScope::default()
        });
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

    Ok(SandboxScope {
        readable: s.readable.iter().map(PathBuf::from).collect(),
        writable: s.writable.iter().map(PathBuf::from).collect(),
        read_all,
        network,
        local_bind: s.local_bind,
        read_deny,
        write_deny,
    })
}

/// Build the coaching hint for a network-denied shell command: if it's
/// compound (cd/&&/pipe) and its first segment alone would match a
/// shell_whitelist pattern, name that pattern explicitly. Otherwise a
/// generic no-match message. Filesystem denials get no whitelist-specific
/// hint (there's no filesystem whitelist mechanism).
fn build_network_denial_hint(cmd: &str, shell_whitelist_patterns: &[String]) -> Option<String> {
    use gait_state::permissions::matching::{split_commands, wildcard_match};

    let segments = split_commands(cmd);
    if segments.len() > 1 {
        if let Some(first) = segments.first() {
            if let Some(pattern) = shell_whitelist_patterns
                .iter()
                .find(|p| wildcard_match(p, first))
            {
                return Some(format!(
                    "sandbox denied network for this command; pattern \"{pattern}\" would grant it, but shell_whitelist doesn't match compound commands (yours has {} segments: \"{cmd}\"). Try running \"{first}\" verbatim, or use the shell tool's cwd param instead of \"cd &&\".",
                    segments.len(),
                ));
            }
        }
        return Some(format!(
            "sandbox denied network for this command; no shell_whitelist entry matches any segment of \"{cmd}\" — only the user can add a sandbox.shell_whitelist entry. Do not retry it; if the task needs network access, say so in your result."
        ));
    }
    Some(format!(
        "sandbox denied network for this command; no shell_whitelist entry matches \"{cmd}\" — only the user can add a sandbox.shell_whitelist entry. Do not retry it; if the task needs network access, say so in your result."
    ))
}

/// Pick the fs backend for this hand call: `BufferedFs` when the spine supplied a
/// prefetch (delegated mode), else `LocalFs` (disk — unchanged behavior).
fn select_fs(impulse: &Impulse) -> std::sync::Arc<dyn FsBackend> {
    match &impulse.prefetch {
        Some(pf) => std::sync::Arc::new(BufferedFs::new(
            std::path::PathBuf::from(&pf.path),
            pf.content.clone().map(|s| s.into_bytes()),
        )),
        None => std::sync::Arc::new(LocalFs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unavailable() -> anyhow::Result<crate::sandbox::SandboxStatus> {
        Ok(crate::sandbox::SandboxStatus::Unavailable {
            reason: "landlock is not enabled".into(),
        })
    }

    #[test]
    fn require_turns_unavailable_into_a_coded_tool_error() {
        let msg = settle_seal(true, unavailable())
            .expect_err("Unavailable must be fatal")
            .to_string();
        assert!(msg.contains("sandbox_required"), "{msg}");
        assert!(msg.contains("landlock is not enabled"), "{msg}");
        // apply returning Err is Unavailable too.
        let msg = settle_seal(true, Err(anyhow::anyhow!("ruleset refused")))
            .expect_err("apply Err must be fatal")
            .to_string();
        assert!(msg.contains("sandbox_required"), "{msg}");
        // A working seal passes through.
        for ok in [
            crate::sandbox::SandboxStatus::Active,
            crate::sandbox::SandboxStatus::Noop,
        ] {
            assert!(settle_seal(true, Ok(ok)).is_ok());
        }
    }

    #[test]
    fn require_false_keeps_unavailable_non_fatal() {
        assert!(matches!(
            settle_seal(false, unavailable()),
            Ok(crate::sandbox::SandboxStatus::Unavailable { .. })
        ));
        assert!(matches!(
            settle_seal(false, Err(anyhow::anyhow!("boom"))),
            Ok(crate::sandbox::SandboxStatus::Unavailable { .. })
        ));
    }

    #[test]
    fn require_comes_from_local_config_only() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!local_sandbox_require(dir.path()), "default is false");
        std::fs::write(
            dir.path().join("gait.jsonc"),
            "{ \"sandbox\": { \"require\": true } }",
        )
        .unwrap();
        assert!(local_sandbox_require(dir.path()));
        // The Impulse has no field that can set it: an Impulse whose requested
        // scope is as permissive as the wire allows does not change the answer.
        let other = tempfile::tempdir().unwrap();
        let _ = build_scope_from_impulse(
            &gait_proto::gait::sandbox::v1::Impulse::default(),
            other.path(),
            other.path(),
        )
        .unwrap();
        assert!(!local_sandbox_require(other.path()));
    }

    #[test]
    fn classify_denial_by_path_matches_glob_and_subpath() {
        let deny = DenyPolicy {
            subpaths: vec![PathBuf::from("/ws/repo/.ssh")],
            globs: vec!["**/.env*".to_string()],
        };
        assert_eq!(
            classify_denial_by_path(Path::new("/ws/repo/.env"), &deny),
            Some("**/.env*".to_string())
        );
        assert_eq!(
            classify_denial_by_path(Path::new("/ws/repo/.ssh/id_rsa"), &deny),
            Some("/ws/repo/.ssh".to_string())
        );
        assert_eq!(
            classify_denial_by_path(Path::new("/ws/repo/src/main.rs"), &deny),
            None
        );
    }

    #[test]
    fn classify_proactive_denial_blocks_read_of_denied_path() {
        use gait_proto::gait::sandbox::v1::Impulse;

        let mut args = pbjson_types::Struct::default();
        args.fields.insert(
            "file_path".into(),
            pbjson_types::Value {
                kind: Some(pbjson_types::value::Kind::StringValue(".env".into())),
            },
        );
        let impulse = Impulse {
            tool: "read".into(),
            args: Some(args),
            requested: None,
            prefetch: None,
            shell_whitelist_patterns: vec![],
            output_capture_mode: gait_proto::gait::sandbox::v1::OutputCaptureMode::Returned as i32,
            output_presentation: gait_proto::gait::sandbox::v1::OutputPresentation::Unspecified
                as i32,
        };
        let codebase = Path::new("/ws/repo");
        let scope = SandboxScope {
            read_deny: DenyPolicy {
                subpaths: vec![],
                globs: vec!["**/.env*".to_string()],
            },
            ..SandboxScope::default()
        };

        let denial = classify_proactive_denial("read", &impulse, codebase, &scope)
            .expect("must classify .env as denied");
        assert_eq!(denial.dimension, "read");
        assert_eq!(denial.pattern, "**/.env*");
        assert_eq!(denial.target, PathBuf::from("/ws/repo/.env"));
    }

    #[test]
    fn classify_proactive_denial_allows_non_denied_read() {
        use gait_proto::gait::sandbox::v1::Impulse;

        let mut args = pbjson_types::Struct::default();
        args.fields.insert(
            "file_path".into(),
            pbjson_types::Value {
                kind: Some(pbjson_types::value::Kind::StringValue("src/main.rs".into())),
            },
        );
        let impulse = Impulse {
            tool: "read".into(),
            args: Some(args),
            requested: None,
            prefetch: None,
            shell_whitelist_patterns: vec![],
            output_capture_mode: gait_proto::gait::sandbox::v1::OutputCaptureMode::Returned as i32,
            output_presentation: gait_proto::gait::sandbox::v1::OutputPresentation::Unspecified
                as i32,
        };
        let codebase = Path::new("/ws/repo");
        let scope = SandboxScope {
            read_deny: DenyPolicy {
                subpaths: vec![],
                globs: vec!["**/.env*".to_string()],
            },
            ..SandboxScope::default()
        };

        assert!(classify_proactive_denial("read", &impulse, codebase, &scope).is_none());
    }

    #[test]
    fn classify_proactive_denial_ignores_shell() {
        use gait_proto::gait::sandbox::v1::Impulse;

        let mut args = pbjson_types::Struct::default();
        args.fields.insert(
            "cmd".into(),
            pbjson_types::Value {
                kind: Some(pbjson_types::value::Kind::StringValue("cat .env".into())),
            },
        );
        let impulse = Impulse {
            tool: "shell".into(),
            args: Some(args),
            requested: None,
            prefetch: None,
            shell_whitelist_patterns: vec![],
            output_capture_mode: gait_proto::gait::sandbox::v1::OutputCaptureMode::Spool as i32,
            output_presentation: gait_proto::gait::sandbox::v1::OutputPresentation::Unspecified
                as i32,
        };
        let codebase = Path::new("/ws/repo");
        let scope = SandboxScope {
            read_deny: DenyPolicy {
                subpaths: vec![],
                globs: vec!["**/.env*".to_string()],
            },
            ..SandboxScope::default()
        };

        // Documented gap: shell isn't proactively gated (see
        // classify_proactive_denial's doc comment) — it can't enumerate the
        // paths an arbitrary shell command might touch without parsing shell
        // syntax, which is out of scope.
        assert!(classify_proactive_denial("shell", &impulse, codebase, &scope).is_none());
    }

    #[test]
    fn build_scope_from_impulse_ignores_wire_supplied_deny() {
        use gait_proto::gait::sandbox::v1::{DenyPolicy as ProtoDenyPolicy, Impulse, Scope};

        let malicious_scope = Scope {
            read_all: false,
            readable: vec![],
            writable: vec![],
            network: None,
            local_bind: false,
            // A compromised/buggy brain tries to ship an empty deny to defeat
            // the floor — this must be ignored, not honored.
            read_deny: Some(ProtoDenyPolicy {
                subpaths: vec![],
                globs: vec![],
            }),
            write_deny: Some(ProtoDenyPolicy {
                subpaths: vec![],
                globs: vec![],
            }),
        };
        let impulse = Impulse {
            tool: "read".into(),
            args: None,
            requested: Some(malicious_scope),
            prefetch: None,
            shell_whitelist_patterns: vec![],
            output_capture_mode: gait_proto::gait::sandbox::v1::OutputCaptureMode::Returned as i32,
            output_presentation: gait_proto::gait::sandbox::v1::OutputPresentation::Unspecified
                as i32,
        };

        // Use a tempdir with a scaffolded gait.jsonc so local_deny_policy has a
        // real config to load (the built-in default deny patterns, since this
        // dir's config was never edited).
        let dir = tempfile::TempDir::new().unwrap();
        gait_ops::workspace::run_init(dir.path()).unwrap();

        let scope = build_scope_from_impulse(&impulse, dir.path(), Path::new("/codebase")).unwrap();

        // The hand must have sourced read_deny/write_deny from its OWN local
        // SandboxConfig (loaded independently, not from `impulse`), not from the
        // (empty, attacker-controlled) wire value above. With no local config
        // override, that means the built-in default secrets patterns are present.
        assert!(
            !scope.read_deny.globs.is_empty(),
            "read_deny must come from local config, not the wire — got empty"
        );
    }

    #[test]
    fn identity_folders_are_denied_to_read_and_write() {
        let dir = tempfile::TempDir::new().unwrap();
        gait_ops::workspace::run_init(dir.path()).unwrap();
        let (read, write) = local_deny_policy(dir.path(), Path::new("/codebase")).unwrap();
        let key = dir.path().join("identity").join("device.jwk.json");
        assert!(classify_denial_by_path(&key, &read).is_some());
        assert!(classify_denial_by_path(&key, &write).is_some());
        let root = dir.path().join("identity-root").join("root.jwk.json");
        assert!(classify_denial_by_path(&root, &read).is_some());
        assert!(classify_denial_by_path(&dir.path().join("notes.txt"), &read).is_none());
    }

    #[test]
    fn machine_identity_dir_is_denied_with_gait_identity_dir_and_deny_env_unset() {
        let dir = tempfile::TempDir::new().unwrap();
        gait_ops::workspace::run_init(dir.path()).unwrap();
        let machine = tempfile::TempDir::new().unwrap();
        let machine_path = machine.path().to_path_buf();
        let var = move |k: &str| {
            (k == "MACHINE_IDENTITY_DIR").then(|| machine_path.clone().into_os_string())
        };
        let (read, write) = local_deny_policy_with(dir.path(), Path::new("/codebase"), var, || {
            gait_sandbox::sandbox::ceiling::native_home().ok()
        })
        .unwrap();
        let key = machine.path().join("services").join("some.jwk.json");
        assert!(
            classify_denial_by_path(&key, &read).is_some(),
            "read of machine dir must be denied"
        );
        assert!(
            classify_denial_by_path(&key, &write).is_some(),
            "write of machine dir must be denied"
        );
        assert!(classify_denial_by_path(&machine.path().join("enrolments.json"), &read).is_some());
    }

    #[tokio::test]
    async fn plain_shell_succeeds_with_only_home_set_and_no_identity() {
        // Standalone gait: no identity dir, no deny env, only HOME. The machine
        // dir lands in the deny list as a path that does not exist; harmless.
        let gait = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let ws = tempfile::TempDir::new().unwrap();
        let home_path = home.path().to_path_buf();
        let var = move |k: &str| (k == "HOME").then(|| home_path.clone().into_os_string());
        let (read_deny, write_deny) = local_deny_policy_with(gait.path(), ws.path(), var, || {
            gait_sandbox::sandbox::ceiling::native_home().ok()
        })
        .unwrap();
        let machine = home.path().join(".local/share/machine-identity");
        assert!(!machine.exists());
        assert!(read_deny.subpaths.contains(&machine));
        let scope = SandboxScope {
            read_deny,
            write_deny,
            read_all: true,
            ..SandboxScope::default()
        };
        let ctx = ToolCtx::new(
            ws.path().to_path_buf(),
            gait.path().to_path_buf(),
            "s".into(),
        )
        .with_scope(scope);
        let tools = builtin_tools(ctx, vec![], vec![], vec![], Repos::production());
        let shell = tools.iter().find(|t| t.name() == "shell").unwrap();
        let rec = Recorder::buffering();
        let out = shell
            .execute(
                serde_json::json!({"cmd": "echo standalone-ok", "reason": "t"}),
                &rec,
            )
            .await
            .expect("plain shell must succeed");
        assert!(out.to_string().contains("standalone-ok"), "{out}");
    }

    #[test]
    fn deny_env_value_from_the_spine_is_honoured() {
        let dir = tempfile::TempDir::new().unwrap();
        gait_ops::workspace::run_init(dir.path()).unwrap();
        let var =
            |k: &str| (k == gait_identity::DENY_PATHS_ENV).then(|| "/spine/a:/spine/m".into());
        let (read, _) = local_deny_policy_with(dir.path(), Path::new("/codebase"), var, || {
            gait_sandbox::sandbox::ceiling::native_home().ok()
        })
        .unwrap();
        assert!(classify_denial_by_path(Path::new("/spine/m/x"), &read).is_some());
    }

    #[test]
    fn default_deny_entries_with_tilde_deny_absolute_home_paths() {
        let gait = tempfile::TempDir::new().unwrap();
        gait_ops::workspace::run_init(gait.path()).unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let home_path = home.path().to_path_buf();
        let var = move |k: &str| (k == "HOME").then(|| home_path.clone().into_os_string());
        let (read, write) =
            local_deny_policy_with(gait.path(), Path::new("/codebase"), var, || {
                gait_sandbox::sandbox::ceiling::native_home().ok()
            })
            .unwrap();
        let denied = [
            home.path().join(".ssh/x"),
            home.path().join(".aws/credentials"),
            home.path().join(".config/gh/hosts.yml"),
        ];
        for p in &denied {
            assert!(
                classify_denial_by_path(p, &read).is_some(),
                "read of {} must be denied by default tilde-deny patterns",
                p.display()
            );
            assert!(
                classify_denial_by_path(p, &write).is_some(),
                "write of {} must be denied by default tilde-deny patterns",
                p.display()
            );
        }
        let plain = home.path().join("notes/plain.txt");
        assert!(
            classify_denial_by_path(&plain, &read).is_none(),
            "read of {} must not be denied",
            plain.display()
        );
        assert!(
            classify_denial_by_path(&plain, &write).is_none(),
            "write of {} must not be denied",
            plain.display()
        );
    }

    #[test]
    fn tilde_deny_entry_without_home_is_an_error_not_dropped() {
        // With no HOME and no platform home, a `~` deny entry must make the
        // policy build fail closed (Err naming the entry) instead of
        // silently matching nothing.
        let gait = tempfile::TempDir::new().unwrap();
        gait_ops::workspace::run_init(gait.path()).unwrap();
        let result = local_deny_policy_with(gait.path(), Path::new("/codebase"), |_| None, || None);
        let err = result.expect_err("unresolved home with `~` deny entries must be an error");
        assert!(
            err.to_string().contains("~/"),
            "error must name the offending `~` entry, got: {err}"
        );
    }

    #[test]
    fn deny_home_unresolved_message_names_entry_and_refuses_the_call() {
        let err = gait_sandbox::sandbox::ceiling::DenyHomeUnresolved {
            entry: "~/.ssh/**".into(),
        };
        let msg = deny_home_unresolved_message(&err);
        assert!(msg.contains("error[deny_home_unresolved]"), "got: {msg}");
        assert!(msg.contains("~/.ssh/**"), "got: {msg}");
        assert!(msg.contains("was not run"), "got: {msg}");
        assert!(msg.contains("HOME"), "got: {msg}");
    }

    #[test]
    fn deny_entries_without_tilde_are_unaffected_without_home() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{ "sandbox": { "deny": { "read": ["$CODEBASE/secret", "/abs/x"], "write": [] } } }"#,
        )
        .unwrap();
        let (read, write) =
            local_deny_policy_with(dir.path(), Path::new("/codebase"), |_| None, || None).unwrap();
        assert!(classify_denial_by_path(Path::new("/codebase/secret/k"), &read).is_some());
        assert!(classify_denial_by_path(Path::new("/codebase/other"), &read).is_none());
        assert!(
            classify_denial_by_path(Path::new("/codebase/secret/k"), &write).is_none(),
            "write deny list is empty, nothing must be denied for write"
        );
    }

    #[test]
    fn select_fs_uses_buffered_when_prefetch_present() {
        use gait_proto::gait::sandbox::v1::{Impulse, Prefetch};
        let with = Impulse {
            tool: "read".into(),
            args: None,
            requested: None,
            prefetch: Some(Prefetch {
                path: "/ws/a.rs".into(),
                content: Some("buf".into()),
            }),
            shell_whitelist_patterns: vec![],
            output_capture_mode: gait_proto::gait::sandbox::v1::OutputCaptureMode::Returned as i32,
            output_presentation: gait_proto::gait::sandbox::v1::OutputPresentation::Unspecified
                as i32,
        };
        assert_eq!(
            select_fs(&with)
                .read_to_string(std::path::Path::new("/ws/a.rs"))
                .unwrap(),
            "buf"
        );

        let without = Impulse {
            prefetch: None,
            ..with.clone()
        };
        // LocalFs on a path that doesn't exist errors — proves it is NOT BufferedFs.
        assert!(select_fs(&without)
            .read_to_string(std::path::Path::new("/ws/a.rs"))
            .is_err());
    }

    #[test]
    fn network_denial_hint_names_matching_whitelist_pattern_for_compound_command() {
        // split_commands does no "cd"-stripping — the first segment here is
        // "flutter test --coverage" itself, which matches the pattern; the
        // compound shape (a trailing "&& echo done") is still what triggers
        // the "doesn't match compound commands" coaching branch.
        let hint = build_network_denial_hint(
            "flutter test --coverage && echo done",
            &["flutter test *".to_string()],
        )
        .expect("expected a hint");
        assert!(
            hint.contains("flutter test *"),
            "hint should name the matching pattern: {hint}"
        );
        assert!(
            hint.contains("cwd"),
            "hint should point at the cwd param: {hint}"
        );
    }

    #[test]
    fn network_denial_hint_is_generic_when_no_pattern_matches() {
        let hint =
            build_network_denial_hint("cd /tmp && curl example.com", &[]).expect("expected a hint");
        assert!(
            hint.contains("no shell_whitelist entry matches"),
            "hint should be the generic no-match message: {hint}"
        );
        assert!(
            !hint.contains("add one to"),
            "the model can't edit sandbox config, so the hint must not tell it to: {hint}"
        );
    }

    #[test]
    fn network_denial_hint_for_single_segment_command_is_generic() {
        let hint = build_network_denial_hint("curl example.com", &["flutter test *".to_string()])
            .expect("expected a hint");
        assert!(
            hint.contains("no shell_whitelist entry matches \"curl example.com\""),
            "single-segment miss should be the generic message: {hint}"
        );
    }
}
