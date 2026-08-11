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
    pub output_path: Option<PathBuf>,
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

    // Computed before ToolCtx so tools (the shell tool in particular) can check
    // a requested `cwd` against scope before committing to it, instead of only
    // finding out when the kernel seal later rejects the subprocess spawn.
    // Pure/side-effect-free (only reads `impulse`) — safe to compute this early;
    // the seal itself still applies at the same point as before, below.
    let seal_scope = build_scope_from_impulse(&impulse, &config.gait_dir);

    let ctx = {
        let mut ctx = ToolCtx::new(
            config.codebase.clone(),
            config.gait_dir.clone(),
            config.session.clone(),
        )
        .with_scope(seal_scope.clone());
        if let Some(path) = config.output_path.clone() {
            ctx = ctx.with_output_path(path);
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
    let tools = builtin_tools(ctx, vec![], vec![], vec![], Repos::production());
    let tool = match tools.iter().find(|t| t.name() == config.tool) {
        Some(t) => t,
        None => respond_err(format!("unknown tool: {}", config.tool)),
    };

    // Schema-guided coercion (string -> scalar, float -> int) needs the tool's
    // parameters schema, so it lives here, after the tool is resolved.
    let args_json = crate::tools::coerce_scalar_params(args_json, tool.parameters());

    // App-level deny-policy gate — check-before-serve, not react-after-fail.
    // The kernel-level (Seatbelt) deny mechanism this was originally meant to
    // pair with (2026-07-25 sandbox-file-deny-policy plan, Task 7) turned out
    // to be unreliable on real hardware (an undocumented macOS sandbox-compiler
    // quirk silently drops trailing `deny` rules in common rule shapes — see
    // that plan's notes) and was reverted; this app-level check is therefore
    // the ONLY thing enforcing `read_deny`/`write_deny` today, not a redundant
    // second layer over working kernel enforcement. If the kernel mechanism is
    // ever fixed and reinstated, this becomes true defense-in-depth on top of
    // it with no changes needed here.
    let proactive_denial =
        classify_proactive_denial(&config.tool, &impulse, &config.codebase, &seal_scope);

    let rec = Recorder::buffering();

    // Apply the seal BEFORE any tool I/O. The hand enforces the kernel sandbox
    // itself — this is irreversible.
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

    let mut result_json = if proactive_denial.is_some() {
        // Do NOT call tool.execute() — the whole point of check-before-serve is
        // that a denied path's content never reaches gait's own code, let alone
        // the model. No disk I/O happens for this call at all.
        serde_json::Value::String(
            "error: denied by sandbox deny policy (matches a configured secrets pattern)"
                .to_string(),
        )
    } else {
        match tool.execute(args_json, &rec).await {
            Ok(v) => v,
            Err(e) => serde_json::Value::String(format!("error: {e}")),
        }
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

    // Preserve the complete result before the model-facing preview crosses IPC.
    // Streaming tools may already have written this exact path; ordinary tools
    // are archived here after execution. Either way, the result sent onward
    // includes the artifact path and stays within the one shared preview budget.
    let output_path = match config.output_path.clone() {
        Some(path) => path,
        None => match crate::tools::prepare_tool_output_path(
            &config.gait_dir,
            &config.session,
            &config.tool,
        ) {
            Ok(path) => path,
            Err(error) => respond_err(format!("failed to prepare tool output artifact: {error}")),
        },
    };
    result_json = match crate::tools::archive_and_prepare_model_value(result_json, output_path) {
        Ok((model_value, _artifact)) => model_value,
        Err(error) => respond_err(format!("failed to archive tool output: {error}")),
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

    let final_content = fs_handle
        .captured_write()
        .map(|b| String::from_utf8_lossy(&b).into_owned());

    let feedback = Feedback {
        result: Some(result_proto),
        events: rec.take_buffer(),
        denied,
        daemon: daemon_record,
        final_content,
        whitelist_match_items: vec![],
        sandbox_denied,
    };
    respond(hand_response::Outcome::Feedback(feedback));
}

// ── build_scope_from_impulse ──────────────────────────────────────────────────

/// Hard floor: read_deny/write_deny are ALWAYS sourced from the hand's own
/// locally-loaded SandboxConfig, never from `impulse.requested` — a brain
/// (sealed, network-facing, distrusted) must not be able to supply or widen
/// these two fields regardless of what it sends over the wire. This is what
/// makes the deny policy a floor rather than a claim.
fn local_deny_policy(gait_dir: &Path) -> (crate::sandbox::DenyPolicy, crate::sandbox::DenyPolicy) {
    let cfg = gait_types::config::load_config(gait_dir);
    let read = gait_sandbox::sandbox::ceiling::split_deny_patterns(&cfg.sandbox.deny.read);
    let write = gait_sandbox::sandbox::ceiling::split_deny_patterns(&cfg.sandbox.deny.write);
    (read, write)
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
pub fn build_scope_from_impulse(impulse: &Impulse, gait_dir: &Path) -> SandboxScope {
    let (read_deny, write_deny) = local_deny_policy(gait_dir);

    let Some(s) = impulse.requested.as_ref() else {
        return SandboxScope {
            read_deny,
            write_deny,
            ..SandboxScope::default()
        };
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
        local_bind: s.local_bind,
        read_deny,
        write_deny,
    }
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
            "sandbox denied network for this command; no shell_whitelist entry matches any segment of \"{cmd}\" — add one to sandbox.shell_whitelist if this should be allowed."
        ));
    }
    Some(format!(
        "sandbox denied network for this command; no shell_whitelist entry matches \"{cmd}\" — add one to sandbox.shell_whitelist if this should be allowed."
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
        };

        // Use a tempdir with a scaffolded gait.jsonc so local_deny_policy has a
        // real config to load (the built-in default deny patterns, since this
        // dir's config was never edited).
        let dir = tempfile::TempDir::new().unwrap();
        gait_ops::workspace::run_init(dir.path()).unwrap();

        let scope = build_scope_from_impulse(&impulse, dir.path());

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
