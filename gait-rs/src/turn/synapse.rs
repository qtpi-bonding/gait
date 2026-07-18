//! The Synapse seam — the motor pathway from a decided tool call to a hand.
//! `InProcessSynapse` runs tools in-process (tests, non-spine paths).
//! `RelaySynapse` fires through the spine over the Nerve (production, Phase 3).
//! Both implement the same `Synapse` trait so the call site in `exec.rs` is
//! unchanged except which impl is constructed.
#![allow(dead_code)] // planned seam: ScopeRequest.access used in Phase 2+ (spine scope enforcement)

/// A short, single-line label for the heartbeat: `"<tool>: <cmd snippet>"`, or
/// just the tool name when there's no `cmd`. Truncated to 60 chars (+ ellipsis).
pub(crate) fn tool_label(name: &str, args: &serde_json::Value) -> String {
    match args.get("cmd").and_then(|v| v.as_str()) {
        Some(cmd) => {
            let one_line = cmd.split('\n').next().unwrap_or(cmd).trim();
            let snippet: String = one_line.chars().take(60).collect();
            let ellipsis = if one_line.chars().count() > 60 {
                "…"
            } else {
                ""
            };
            format!("{name}: {snippet}{ellipsis}")
        }
        None => name.to_string(),
    }
}

use crate::proto::gait::log::v1::Event;
use crate::proto::gait::log::v1::SandboxWhitelistMatchItem;
use crate::proto::gait::log::v1::UnlockItem;
use crate::proto::Recorder;
use crate::tools::ToolError;
use crate::types::{Tool, ToolAccess};

/// What the brain requests for a tool call. Phase 1 carries only the access
/// dimensions; Phase 2 adds resolved paths + network targets bounded by the spine.
#[derive(Debug, Clone)]
pub struct ScopeRequest {
    pub access: ToolAccess,
}

/// The motor pathway from a decided tool call to a hand: run the tool and return
/// `{result, events, denied, whitelist_match_items}`. Implementations differ by WHERE the tool runs (in-process
/// now; sealed subprocess / spine relay later) but share this contract.
/// Returns `Vec<Event>` (not `Vec<event::Body>`) — Event is serde-derivable via
/// pbjson, so Phase 2a can pipe the events over the wire directly.
///
/// The third element (`Option<UnlockItem>`) is `Some` when the hand detected a
/// kernel-level permission denial (EACCES / EPERM) while executing a `shell`
/// command under Seatbelt. The turn loop inspects this in supervised mode and
/// calls `pause_for_unlock`; in auto mode it is ignored and the tool failure
/// result propagates normally (Phase 2c Task 6).
#[async_trait::async_trait]
pub trait Synapse: Send + Sync {
    async fn fire(
        &self,
        tool: &dyn Tool,
        args: serde_json::Value,
        scope: ScopeRequest,
    ) -> Result<
        (
            serde_json::Value,
            Vec<Event>,
            Option<UnlockItem>,
            Vec<SandboxWhitelistMatchItem>,
        ),
        ToolError,
    >;
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
    ) -> Result<
        (
            serde_json::Value,
            Vec<Event>,
            Option<UnlockItem>,
            Vec<SandboxWhitelistMatchItem>,
        ),
        ToolError,
    > {
        let buf = Recorder::buffering();
        let result = tool.execute(args, &buf).await?;
        Ok((result, buf.take_buffer(), None, vec![]))
    }
}

// ── RelaySynapse ──────────────────────────────────────────────────────────────

/// Phase 3 impl: send an `Impulse` Signal to the spine via the Nerve and await
/// the matching `Feedback` Signal by `corr_id`. The spine bounds the scope,
/// spawns the sealed hand, and returns `{result, events, denied}`.
///
/// Selected in `exec.rs` when the brain has a relay handle. When there is no
/// relay (tests, direct `cargo test`), `InProcessSynapse` is used instead.
pub struct RelaySynapse {
    pub writer: crate::turn::nerve::NerveWriter,
}

#[async_trait::async_trait]
impl Synapse for RelaySynapse {
    async fn fire(
        &self,
        tool: &dyn Tool,
        args: serde_json::Value,
        _scope: ScopeRequest,
    ) -> Result<
        (
            serde_json::Value,
            Vec<Event>,
            Option<UnlockItem>,
            Vec<SandboxWhitelistMatchItem>,
        ),
        ToolError,
    > {
        crate::diagnostic::set_phase_labeled(
            crate::diagnostic::Phase::AwaitFeedback,
            Some(tool_label(tool.name(), &args)),
        );

        // Convert args to proto Struct for the Impulse.
        let args_struct: pbjson_types::Struct = serde_json::from_value(args).unwrap_or_default();

        let corr_id = self.writer.alloc_corr_id();
        let signal = gait_proto::gait::sandbox::v1::Signal {
            corr_id,
            body: Some(gait_proto::gait::sandbox::v1::signal::Body::Impulse(
                gait_proto::gait::sandbox::v1::Impulse {
                    tool: tool.name().to_string(),
                    args: Some(args_struct),
                    cwd: String::new(), // spine reads cwd from session for hands
                    requested: None,    // spine re-derives scope via bound_request
                    prefetch: None,     // no delegation from this code path
                },
            )),
        };

        let rx = self
            .writer
            .send_and_register(&signal)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("nerve send: {e}")))?;

        let response = rx
            .await
            .map_err(|_| ToolError::Internal(anyhow::anyhow!("nerve: spine dropped reply")))?;

        let feedback = match response.body {
            Some(gait_proto::gait::sandbox::v1::signal::Body::Feedback(f)) => f,
            other => {
                return Err(ToolError::Internal(anyhow::anyhow!(
                    "nerve: expected Feedback, got {other:?}"
                )));
            }
        };

        let result_json = feedback
            .result
            .as_ref()
            .and_then(|v| serde_json::to_value(v).ok())
            .unwrap_or(serde_json::Value::Null);

        Ok((
            result_json,
            feedback.events,
            feedback.denied,
            feedback.whitelist_match_items,
        ))
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
            rec: &dyn crate::types::Rec,
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
        let (result, events, denied, whitelist_items) = syn
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
        assert!(denied.is_none(), "in-process synapse never signals denial");
        assert!(
            whitelist_items.is_empty(),
            "in-process synapse is unsandboxed — whitelist matching never applies"
        );
    }

    /// RelaySynapse: a fake spine reads the Impulse from the Nerve and echoes back
    /// a canned Feedback. Verifies the brain-side demux returns the right result.
    #[tokio::test]
    async fn relay_synapse_fake_spine_returns_feedback() {
        use crate::turn::nerve;
        use gait_proto::gait::sandbox::v1::{signal, Feedback};

        let (brain_read, spine_write_half) = tokio::io::duplex(4096);
        let (spine_read_half, brain_write) = tokio::io::duplex(4096);
        let (writer, _handle) = nerve::start_nerve(brain_read, brain_write);

        // Fake spine task: read one Impulse and echo a canned Feedback.
        tokio::spawn(async move {
            let mut r = spine_read_half;
            let mut w = spine_write_half;
            let sig = nerve::read_signal(&mut r).await.unwrap();
            let canned = gait_proto::gait::sandbox::v1::Signal {
                corr_id: sig.corr_id,
                body: Some(signal::Body::Feedback(Feedback {
                    result: Some(pbjson_types::Value {
                        kind: Some(pbjson_types::value::Kind::StringValue(
                            "relay-ok".to_string(),
                        )),
                    }),
                    events: vec![],
                    denied: None,
                    daemon: None,
                    final_content: None,
                    whitelist_match_items: vec![],
                })),
            };
            nerve::write_signal(&mut w, &canned).await.unwrap();
        });

        let syn = RelaySynapse { writer };
        let (result, events, denied, whitelist_items) = syn
            .fire(
                &EchoTool,
                serde_json::json!({"k": "v"}),
                ScopeRequest {
                    access: EchoTool.access(),
                },
            )
            .await
            .unwrap();

        assert_eq!(result.as_str().unwrap_or(""), "relay-ok");
        assert!(events.is_empty());
        assert!(denied.is_none());
        assert!(whitelist_items.is_empty());
    }

    #[test]
    fn tool_label_includes_cmd_snippet() {
        let args = serde_json::json!({"cmd": "flutter pub get", "reason": "x"});
        assert_eq!(tool_label("shell", &args), "shell: flutter pub get");
    }

    #[test]
    fn tool_label_truncates_long_single_line() {
        let long = format!("echo {}", "a".repeat(100));
        let args = serde_json::json!({"cmd": long});
        let l = tool_label("shell", &args);
        assert!(l.starts_with("shell: echo "), "got: {l}");
        assert!(l.ends_with('…'), "should be truncated: {l}");
        // "shell: " (7) + 60 chars + "…"
        assert_eq!(l.chars().count(), 7 + 60 + 1, "got: {l}");
    }

    #[test]
    fn tool_label_uses_first_line_only() {
        let args = serde_json::json!({"cmd": "set -e\nflutter pub get"});
        assert_eq!(tool_label("shell", &args), "shell: set -e");
    }

    #[test]
    fn tool_label_without_cmd_is_tool_name() {
        let args = serde_json::json!({"path": "foo.rs"});
        assert_eq!(tool_label("read", &args), "read");
    }
}
