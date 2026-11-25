//! Red-step tests for the REPL-injected gated workflow tool (tests only —
//! the feature API they exercise does not exist yet, so this file must fail
//! to compile until the feature lands).
//!
//! Covered here:
//! 1. `ResolvedProfile::add_gated_tool` registers a tool WITHOUT touching the
//!    allow list, and the pure gate classifier still routes the tool's calls
//!    to `GateDecision::Gate`.
//! 2. The workspace guard `tests/workflow_runner_removed.rs` keeps its
//!    needle spellings (`trait WorkflowRunner`, `with_workflow_runner`,
//!    `workflow_runner::`) and keeps NOT matching `RunWorkflowTool`.
//! 3. `TurnRunOpts.workflow_requests = None` is constructible (existing
//!    behavior unchanged), and `Some(tx)` round-trips a
//!    `WorkflowRequest`/`WorkflowReply` through the channel.

use std::path::PathBuf;
use std::sync::Arc;

use gait_core::provider::Provider;
use gait_core::types::{PermissionRequest, Tool, ToolCall, ToolPermission};
use gait_turn_runner::{TurnRunOpts, WorkflowReply, WorkflowRequest};

/// A minimal fake provider so `ResolvedProfile::for_test` has something to
/// hold. Never actually streamed from — these tests never run a turn.
struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    async fn stream(
        &self,
        _system: &str,
        _messages: &[gait_core::types::Message],
        _tools: &[Arc<dyn Tool>],
        _params: &gait_core::config::LlmParamsConfig,
        _tail_context: Option<&str>,
    ) -> Result<gait_core::provider::ChunkStream, gait_core::provider::ProviderError> {
        Err(gait_core::provider::ProviderError::Other(
            "stub provider: never streamed".to_string(),
        ))
    }

    fn model_id(&self) -> &str {
        "stub/model"
    }
}

/// A fake tool whose `permission()` reports `RequiresApproval` with one
/// request — the shape a gated workflow tool would present to the gate.
struct FakeGatedTool {
    tool_name: String,
}

#[async_trait::async_trait]
impl Tool for FakeGatedTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        "fake gated tool"
    }

    fn parameters(&self) -> &serde_json::Value {
        &serde_json::Value::Null
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _rec: &dyn gait_core::types::Rec,
    ) -> Result<serde_json::Value, gait_core::types::ToolError> {
        Ok(serde_json::Value::Null)
    }

    fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: "run_workflow".into(),
            patterns: vec!["run_workflow".into()],
        }])
    }
}

#[test]
fn gated_tool_is_not_in_the_allow_list_and_triggers_the_permission_gate() {
    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let mut profile = gait_core::profile::ResolvedProfile::for_test(
        provider,
        vec![],
        vec!["read".into()],
        vec![],
    );

    let gated = Arc::new(FakeGatedTool {
        tool_name: "run_workflow".into(),
    });
    profile.add_gated_tool(gated);

    let names: Vec<&str> = profile
        .registered_tools()
        .iter()
        .map(|t| t.name())
        .collect();
    assert!(
        names.contains(&"run_workflow"),
        "the gated tool must be registered, got {names:?}"
    );
    assert!(
        !profile.allow_list().contains(&"run_workflow".to_string()),
        "add_gated_tool must not touch the allow list, got {:?}",
        profile.allow_list()
    );
    assert_eq!(profile.allow_list(), &["read".to_string()]);

    // The pure gate classifier must route a call to the gated tool to the
    // permission gate: it is registered but not allow-listed.
    let tc = ToolCall {
        tool_call_id: "tc-gated".into(),
        tool_name: "run_workflow".into(),
        args: serde_json::json!({}),
    };
    let gated_ref: &dyn Tool = &FakeGatedTool {
        tool_name: "run_workflow".into(),
    };
    let decision = gait_core::permissions::gate::classify(
        &tc,
        Some(gated_ref),
        profile.allow_list(),
        profile.deny_list(),
        false,
    );
    assert!(
        matches!(
            decision,
            gait_core::permissions::gate::GateDecision::Gate { .. }
        ),
        "a registered-but-unallow-listed tool must classify as Gate"
    );

    // Contrast: add_allowed_tool (the existing API) DOES add to the allow
    // list, so the same classifier returns Allow for it.
    let allowed = Arc::new(FakeGatedTool {
        tool_name: "plain_tool".into(),
    });
    profile.add_allowed_tool(allowed);
    assert!(
        profile.allow_list().contains(&"plain_tool".to_string()),
        "add_allowed_tool must add to the allow list, got {:?}",
        profile.allow_list()
    );
    let tc_allowed = ToolCall {
        tool_call_id: "tc-allowed".into(),
        tool_name: "plain_tool".into(),
        args: serde_json::json!({}),
    };
    let plain_ref: &dyn Tool = &FakeGatedTool {
        tool_name: "plain_tool".into(),
    };
    let allowed_decision = gait_core::permissions::gate::classify(
        &tc_allowed,
        Some(plain_ref),
        profile.allow_list(),
        profile.deny_list(),
        false,
    );
    assert!(
        matches!(
            allowed_decision,
            gait_core::permissions::gate::GateDecision::Allow { .. }
        ),
        "an allow-listed tool must classify as Allow"
    );
}

#[test]
fn guard_test_still_forbids_the_other_removed_names() {
    let guard_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/workflow_runner_removed.rs");
    let text = std::fs::read_to_string(&guard_path)
        .expect("workspace guard file must exist next to the workspace root");

    // The guard file builds its needles with concat!(...), so the joined
    // strings never appear literally in it. We assert on the concat! source
    // forms instead, built with format! from the same fragments so this
    // test file never self-matches either.
    let trait_needle = format!("concat!(\"{}\", \"{}\")", "trait ", "WorkflowRunner");
    let with_runner_needle = format!("concat!(\"{}\", \"{}\")", "with_", "workflow_runner");
    let module_needle = format!("concat!(\"{}\", \"{}\")", "workflow_", "runner::");
    let removed_tool_needle = format!("concat!(\"{}\", \"{}\")", "Run", "WorkflowTool");

    assert!(
        text.contains(&trait_needle),
        "guard must still forbid {trait_needle:?}"
    );
    assert!(
        text.contains(&with_runner_needle),
        "guard must still forbid {with_runner_needle:?}"
    );
    assert!(
        text.contains(&module_needle),
        "guard must still forbid {module_needle:?}"
    );
    assert!(
        !text.contains(&removed_tool_needle),
        "RunWorkflowTool must already be deleted, not just guarded"
    );
}

#[tokio::test]
async fn none_channel_leaves_existing_turn_behavior_unchanged() {
    let gait_dir = PathBuf::from(".");
    // Field list copied from gait-turn-runner's run_turn_source.rs test_opts,
    // extended with the new workflow_requests channel.
    let opts_none = TurnRunOpts {
        run_id: "run-none".to_string(),
        session_id: "s-none".to_string(),
        continue_session: "s-none".to_string(),
        gait_dir: gait_dir.clone(),
        codebase: gait_dir.clone(),
        args: vec![],
        sandbox_on: None,
        cascade_group_arg: None,
        cascade: None,
        profile: None,
        fs_delegate: None,
        allow_intree: true,
        workflow_requests: None,
    };
    assert!(
        opts_none.workflow_requests.is_none(),
        "None channel must be constructible and read back as None"
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let opts_some = TurnRunOpts {
        run_id: "run-some".to_string(),
        session_id: "s-some".to_string(),
        continue_session: "s-some".to_string(),
        gait_dir: gait_dir.clone(),
        codebase: gait_dir.clone(),
        args: vec![],
        sandbox_on: None,
        cascade_group_arg: None,
        cascade: None,
        profile: None,
        fs_delegate: None,
        allow_intree: true,
        workflow_requests: Some(tx),
    };

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<WorkflowReply>();
    let request = WorkflowRequest {
        name: "my_workflow".to_string(),
        args: vec!["--flag".to_string()],
        reply: reply_tx,
    };
    let sender = opts_some
        .workflow_requests
        .as_ref()
        .expect("Some(tx) must be set on the second opts");
    sender
        .send(request)
        .await
        .expect("workflow request must send through the channel");

    let received = rx
        .recv()
        .await
        .expect("the workflow request must arrive on rx");
    assert_eq!(received.name, "my_workflow");
    assert_eq!(received.args, vec!["--flag".to_string()]);

    received
        .reply
        .send(WorkflowReply {
            run_id: "wf-run-1".to_string(),
            outcome: "completed".to_string(),
            steps: vec!["step-a".to_string(), "step-b".to_string()],
        })
        .expect("the handler must answer the oneshot");

    let answer = reply_rx
        .await
        .expect("the requester must get the reply back");
    assert_eq!(answer.run_id, "wf-run-1");
    assert_eq!(answer.outcome, "completed");
    assert_eq!(
        answer.steps,
        vec!["step-a".to_string(), "step-b".to_string()]
    );
}
