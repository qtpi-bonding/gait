//! The Synapse seam — the motor pathway from a decided tool call to a hand.
//! Phase 1 ships `InProcessSynapse` (runs in-process, identical behavior);
//! Phase 2a adds `SubprocessSynapse` (spawns `gait __run-tool` as a sealed hand
//! and crosses the binary `Impulse`/`Feedback` protobuf hop); Phase 3 layers the
//! spine-relay impl behind the same trait.
#![allow(dead_code)] // planned seam: ScopeRequest.access used in Phase 2+ (spine scope enforcement)

use crate::proto::gait::log::v1::Event;
use crate::proto::Recorder;
use crate::sandbox::SandboxScope;
use crate::tools::ToolError;
use crate::types::{Tool, ToolAccess};
use gait_proto::gait::sandbox::v1::{network_scope::Mode, Feedback, Impulse, NetworkScope, Scope};
use prost::Message;
use tokio::io::AsyncWriteExt;

/// What the brain requests for a tool call. Phase 1 carries only the access
/// dimensions; Phase 2 adds resolved paths + network targets bounded by the spine.
#[derive(Debug, Clone)]
pub struct ScopeRequest {
    pub access: ToolAccess,
}

/// The motor pathway from a decided tool call to a hand: run the tool and return
/// `{result, events}`. Implementations differ by WHERE the tool runs (in-process
/// now; sealed subprocess / spine relay later) but share this contract.
/// Returns `Vec<Event>` (not `Vec<event::Body>`) — Event is serde-derivable via
/// pbjson, so Phase 2a can pipe the events over the wire directly.
#[async_trait::async_trait]
pub trait Synapse: Send + Sync {
    async fn fire(
        &self,
        tool: &dyn Tool,
        args: serde_json::Value,
        scope: ScopeRequest,
    ) -> Result<(serde_json::Value, Vec<Event>), ToolError>;
}

/// Phase 1 impl: run the tool in-process against a buffering recorder so emitted
/// events are returned (not written live) — establishing the exact contract the
/// subprocess/relay impls will satisfy. Behavior-identical once the caller records
/// the returned events.
pub struct InProcessSynapse;

#[async_trait::async_trait]
impl Synapse for InProcessSynapse {
    async fn fire(
        &self,
        tool: &dyn Tool,
        args: serde_json::Value,
        _scope: ScopeRequest,
    ) -> Result<(serde_json::Value, Vec<Event>), ToolError> {
        let buf = Recorder::buffering();
        let result = tool.execute(args, &buf).await?;
        Ok((result, buf.take_buffer()))
    }
}

/// Phase 2a impl: spawn `gait __run-tool` as a child process, encode the call as a
/// binary `Impulse` on stdin, decode the binary `Feedback` from stdout. This is the
/// first out-of-process hand: same `Synapse` contract as `InProcessSynapse`, but the
/// tool runs in a fresh process whose entire input/output crosses the wire.
///
/// The hand is a *sealed* binary — it has no terminal, no REPL, no transcript of its
/// own — so the subprocess hop is what guarantees the tool can't reach brain-side
/// state. Phase 2b will move the actual routing decision into `turn.rs`; this struct
/// only knows how to fire one call.
pub struct SubprocessSynapse {
    pub gait_dir: std::path::PathBuf,
    pub codebase: std::path::PathBuf,
    pub session_id: String,
    pub cwd: String,
    /// Sandbox config used to compute the per-call scope ceiling.
    pub sandbox_cfg: crate::config::SandboxConfig,
    /// When false the hand skips `platform_sandbox().apply()` (NoopSandbox). Controlled
    /// by `--sandbox`/`--no-sandbox` CLI flags or the `/sandbox on|off` REPL toggle.
    pub seal: bool,
}

/// Serialize a `SandboxScope` into the proto `Scope` carried by `Impulse.requested`.
/// Paths are lossy-stringified (matches Phase 2a wire format). Phase 2d will tighten
/// read_all to false and emit per-path subpath rules; this helper stays stable.
fn scope_to_proto(s: &SandboxScope) -> Scope {
    use crate::sandbox::NetworkPolicy;
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

#[async_trait::async_trait]
impl Synapse for SubprocessSynapse {
    async fn fire(
        &self,
        tool: &dyn Tool,
        args: serde_json::Value,
        scope: ScopeRequest,
    ) -> Result<(serde_json::Value, Vec<Event>), ToolError> {
        // Convert serde_json::Value → pbjson_types::Struct.
        // pbjson_types::Struct implements serde Deserialize, so a round-trip via JSON
        // is the cleanest portable conversion. An unparseable value collapses to an
        // empty object — the hand can still see the call.
        let args_struct: pbjson_types::Struct = serde_json::from_value(args).unwrap_or_default();

        // Phase 2b: compute proper per-tool scope ceiling.
        // Create a fresh per-runner temp jail (canonicalize handles /tmp → /private/tmp on macOS)
        let jail_raw = std::env::temp_dir().join(format!(
            "gait-hand-{}",
            uuid::Uuid::new_v4().to_string().replace('-', "")
        ));
        std::fs::create_dir_all(&jail_raw)
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("create temp jail: {e}")))?;
        let temp_jail = jail_raw.canonicalize().unwrap_or_else(|_| jail_raw.clone());

        let sand_scope = crate::sandbox::ceiling::compute_scope(
            scope.access,
            &self.sandbox_cfg,
            &self.codebase,
            &temp_jail, // <-- real canonical path now
        );
        let requested = scope_to_proto(&sand_scope);

        let impulse = Impulse {
            tool: tool.name().to_string(),
            args: Some(args_struct),
            cwd: self.cwd.clone(),
            requested: Some(requested),
        };
        let bytes = impulse.encode_to_vec();

        // Spawn the hand. The same binary (`current_exe`) runs the hidden `__run-tool`
        // subcommand; `--gait-dir` / `--codebase` / `--session` / `--tool` / `--temp-jail`
        // tell it which tool instance to execute inside its own `ToolCtx`.
        let exe = std::env::current_exe()
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("current_exe: {e}")))?;
        // Scrub the hand's environment so it cannot read API keys or other
        // brain-side secrets from the process environment. The allowlist mirrors
        // the shell tool's own spawn guard; extended in Phase 2b when the spine
        // relay enforces a per-runner environment capsule.
        let mut child = tokio::process::Command::new(&exe)
            .arg("__run-tool")
            .arg("--gait-dir")
            .arg(&self.gait_dir)
            .arg("--codebase")
            .arg(&self.codebase)
            .arg("--session")
            .arg(&self.session_id)
            .arg("--tool")
            .arg(tool.name())
            .arg("--temp-jail")
            .arg(&temp_jail)
            .arg("--seal")
            .arg(if self.seal { "true" } else { "false" })
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
            // Override TMPDIR so the hand's subprocesses (e.g. `mktemp`) cannot
            // escape the per-runner jail via the host's /tmp.
            .env("TMPDIR", &temp_jail)
            .spawn()
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("spawn __run-tool: {e}")))?;

        // Write the binary Impulse to stdin, then drop the handle so the pipe closes
        // and the hand reads EOF (allowing it to write its Feedback on stdout). Do
        // NOT call `take()` again on `child.stdin` after this — it's `None`.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::Internal(anyhow::anyhow!("hand stdin already taken")))?;
        stdin
            .write_all(&bytes)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("write Impulse: {e}")))?;
        drop(stdin);

        let out = child
            .wait_with_output()
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("wait __run-tool: {e}")))?;

        // Cleanup temp jail (best-effort; don't fail the tool call if this errors)
        let _ = std::fs::remove_dir_all(&temp_jail);

        if !out.status.success() {
            return Err(ToolError::Internal(anyhow::anyhow!(
                "hand exited {}: stderr={}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            )));
        }

        let feedback = Feedback::decode(out.stdout.as_slice())
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("decode Feedback: {e}")))?;

        // pbjson_types::Value → serde_json::Value (any non-Value variant collapses to Null).
        let result_json = feedback
            .result
            .as_ref()
            .and_then(|v| serde_json::to_value(v).ok())
            .unwrap_or(serde_json::Value::Null);

        Ok((result_json, feedback.events))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Tool, ToolAccess};

    struct EchoTool;
    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            ""
        }
        fn parameters(&self) -> &serde_json::Value {
            static P: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            P.get_or_init(|| serde_json::json!({"type":"object"}))
        }
        async fn execute(
            &self,
            args: serde_json::Value,
            rec: &crate::proto::Recorder,
        ) -> Result<serde_json::Value, crate::tools::ToolError> {
            rec.record(crate::proto::events::cwd("/seen")).ok();
            Ok(args)
        }
        fn access(&self) -> ToolAccess {
            ToolAccess {
                reads: true,
                ..Default::default()
            }
        }
    }

    #[tokio::test]
    async fn in_process_synapse_returns_result_and_buffered_events() {
        let syn = InProcessSynapse;
        let (result, events) = syn
            .fire(
                &EchoTool,
                serde_json::json!({"k":"v"}),
                ScopeRequest {
                    access: EchoTool.access(),
                },
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({"k":"v"}));
        assert_eq!(
            events.len(),
            1,
            "the tool's cwd event is captured, not written live"
        );
        assert!(
            events[0].body.is_some(),
            "buffered event must carry its body"
        );
    }

    /// Documents the full subprocess round-trip. Requires the `gait` binary to be
    /// built first (`cargo build -p gait-bin`) — under `cargo test -p gait-core`
    /// alone the binary at `current_exe()` is a test harness that lacks the
    /// `__run-tool` subcommand, so the test is `#[ignore]`d. Run manually with:
    ///
    ///     cargo build -p gait-bin
    ///     cargo test -p gait-core -- --ignored subprocess_synapse
    ///
    /// The Task 2 integration test (`tests/run_tool_hand.rs`) already covers this
    /// path end-to-end without the ignore, so the unit-test variant here exists to
    /// pin the API and document Phase 2b's intent.
    #[tokio::test]
    #[ignore = "requires `gait` binary built; the end-to-end path is covered by tests/run_tool_hand.rs"]
    async fn subprocess_synapse_runs_read_tool() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "payload-xyz").unwrap();

        let syn = SubprocessSynapse {
            gait_dir: dir.path().to_path_buf(),
            codebase: dir.path().to_path_buf(),
            session_id: "stest123".into(),
            cwd: dir.path().to_string_lossy().into_owned(),
            sandbox_cfg: crate::config::SandboxConfig::default(),
            seal: true,
        };
        let tool = crate::tools::read::ReadTool::new(crate::tools::ToolCtx::new(
            dir.path().into(),
            dir.path().into(),
            "stest123".into(),
        ));
        let (result, _events) = syn
            .fire(
                &tool,
                serde_json::json!({"file_path": "f.txt"}),
                ScopeRequest {
                    access: tool.access(),
                },
            )
            .await
            .unwrap();
        assert!(
            result.as_str().unwrap_or("").contains("payload-xyz"),
            "file content must survive the subprocess hop; got: {:?}",
            result
        );
    }
}
