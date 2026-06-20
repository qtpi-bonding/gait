//! The Synapse seam — the motor pathway from a decided tool call to a hand.
//! Phase 1 ships only `InProcessSynapse` (runs in-process, identical behavior);
//! Phase 2/3 add subprocess + spine-relay impls behind the same trait.
#![allow(dead_code)] // planned seam: ScopeRequest.access used in Phase 2+ (spine scope enforcement)

use crate::proto::gait::log::v1::Event;
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
}
