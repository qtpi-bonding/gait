//! Proof-of-concept for a wayfinder-esque decision-resolution workflow
//! (docs/superpowers/specs/2026-09-05-wayfinder-grorg-workflow-design.md):
//! one "ticket" resolved via prompt -> branch -> (gate) -> shell, with the
//! grorg submission step stubbed as `echo grorg ...` (no real grorg calls).
//!
//! Exercises, against the REAL runner (not mocked): a `prompt` node forced
//! to structured `output_schema` JSON, a second `prompt` node continuing
//! the first via `continue_session`, a multi-way `branch` (`cases` + `else`)
//! routing on the first node's structured field, a `gate` pause/resume on
//! the HITL arm, straight-through completion on the AFK arm, and `shell`
//! interpolation of `$node.output.<field>` from two different prompt nodes.
//!
//! Gated like tests/integration_anthropic.rs: skips (does not fail) when
//! ANTHROPIC_API_KEY is unset, since it makes real model calls.

use gait_workflow::convert::to_proto;
use gait_workflow::observer::CliObserver;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef;
use gait_workflow::state::{reconstruct_state, NodeStatus};

use gait_core::proto::gait::workflow::v1::workflow_event;
use tempfile::TempDir;

fn init_git(gait_dir: &std::path::Path) {
    std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(gait_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(gait_dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t.com")
        .output()
        .unwrap();
}

/// `hint` pins the model's `ticket_type` choice so the branch arm taken is
/// deterministic instead of left to the model's judgment.
fn workflow_yaml(hint: &str) -> String {
    format!(
        r#"
name: wayfinder-poc
model: anthropic/claude-haiku-4-5
auto: true

nodes:
  - id: grill
    prompt: |
      You are resolving one decision ticket from a work-planning map,
      wayfinder-style. The decision to grill: "Should the new caching layer
      use Redis or an in-process LRU cache?" Pick an answer and write one
      sentence explaining it as decision_gist.
      Set ticket_type to exactly "{hint}" for this exercise (ignore whether
      that's the type you'd naturally pick). Set out_of_scope to false.
    output_schema:
      type: object
      required: [ticket_type, decision_gist, out_of_scope]
      properties:
        ticket_type:
          type: string
          enum: [grilling, prototype, research, task]
        decision_gist:
          type: string
        out_of_scope:
          type: boolean

  - id: domain-model
    depends_on: [grill]
    continue_session: grill
    prompt: |
      Given the decision you just made, list 0 to 2 short new child-ticket
      titles it spawns (an empty list is fine).
    output_schema:
      type: object
      required: [new_children]
      properties:
        new_children:
          type: array
          items:
            type: string

  - id: route
    depends_on: [domain-model]
    branch:
      on: "$grill.output.ticket_type"
      cases:
        grilling: gate
        prototype: gate
        research: resolve-afk
        task: resolve-afk
      else: resolve-afk

  - id: gate
    depends_on: [route]
    gate: "Review the grilled decision, then run: gait workflow continue $WORKFLOW_ID"

  - id: resolve-hitl
    depends_on: [gate]
    shell: "echo grorg action_update decision=\"$grill.output.decision_gist\" children=\"$domain-model.output.new_children\""

  - id: resolve-afk
    depends_on: [route]
    shell: "echo grorg action_update decision=\"$grill.output.decision_gist\" children=\"$domain-model.output.new_children\""
"#
    )
}

fn require_anthropic_key() -> Option<String> {
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) => Some(k),
        Err(_) => {
            eprintln!("ANTHROPIC_API_KEY not set, skipping wayfinder_poc test");
            None
        }
    }
}

fn node_complete_output(events: &[gait_core::proto::gait::workflow::v1::WorkflowEvent], node_id: &str) -> Option<String> {
    events.iter().find_map(|e| match &e.kind {
        Some(workflow_event::Kind::NodeComplete(nc)) if nc.node_id == node_id => {
            Some(nc.output.clone())
        }
        _ => None,
    })
}

#[tokio::test]
async fn hitl_arm_gates_then_resumes_with_spliced_output() {
    if require_anthropic_key().is_none() {
        return;
    }

    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git(&gait_dir);

    let yaml = workflow_yaml("grilling");
    let wf: WorkflowDef = serde_yaml::from_str(&yaml).unwrap();
    let run_id = "wayfinder-poc-hitl-001".to_string();

    // --- First run: must pause at the gate, not complete.
    let mut sink = gait_core::embed::CliSink::stdio(true);
    let first = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "wayfinder-poc.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
            ..Default::default()
        },
        &mut sink,
        &mut CliObserver,
        None,
    )
    .await
    .expect("first run should pause cleanly at the gate");
    assert_eq!(first, WorkflowOutcome::GatePaused);

    let run_log = WorkflowRunLog::open(&gait_dir.join("workflow-runs"), &run_id).unwrap();
    let events_before_resume = run_log.read_events().unwrap();
    let state_before_resume = reconstruct_state(&events_before_resume);
    assert_eq!(
        state_before_resume.node_status("resolve-hitl"),
        NodeStatus::Pending,
        "resolve-hitl must not have run before the gate is cleared"
    );

    // --- Resume: same run_id, same graph -- what `gait workflow continue` does.
    let mut sink2 = gait_core::embed::CliSink::stdio(true);
    let second = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "wayfinder-poc.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
            ..Default::default()
        },
        &mut sink2,
        &mut CliObserver,
        None,
    )
    .await
    .expect("resume should complete the workflow");
    assert_eq!(second, WorkflowOutcome::Completed);

    let events = run_log.read_events().unwrap();
    let state = reconstruct_state(&events);

    assert_eq!(state.node_status("grill"), NodeStatus::Done);
    assert_eq!(state.node_status("domain-model"), NodeStatus::Done);
    assert_eq!(state.node_status("route"), NodeStatus::Done);
    assert_eq!(state.node_status("gate"), NodeStatus::Done);
    assert_eq!(
        state.node_status("resolve-hitl"),
        NodeStatus::Done,
        "the HITL arm must have actually run post-resume"
    );
    assert_eq!(
        state.node_status("resolve-afk"),
        NodeStatus::Skipped,
        "the AFK arm must be skipped -- branch selected the other arm"
    );

    let hitl_output = node_complete_output(&events, "resolve-hitl")
        .expect("resolve-hitl must have a captured shell output");
    assert!(
        hitl_output.contains("grorg action_update"),
        "shell node did not echo the stand-in grorg command: {hitl_output:?}"
    );
    assert!(
        !hitl_output.contains("$grill.output.decision_gist"),
        "decision_gist variable was not spliced in: {hitl_output:?}"
    );
    assert!(
        !hitl_output.contains("$domain-model.output.new_children"),
        "new_children variable (from the continue_session node) was not spliced in: {hitl_output:?}"
    );
}

#[tokio::test]
async fn afk_arm_skips_gate_and_completes_in_one_run() {
    if require_anthropic_key().is_none() {
        return;
    }

    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git(&gait_dir);

    let yaml = workflow_yaml("research");
    let wf: WorkflowDef = serde_yaml::from_str(&yaml).unwrap();
    let run_id = "wayfinder-poc-afk-001".to_string();

    let mut sink = gait_core::embed::CliSink::stdio(true);
    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "wayfinder-poc.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
            ..Default::default()
        },
        &mut sink,
        &mut CliObserver,
        None,
    )
    .await
    .expect("AFK arm should complete in a single run, no gate to pause at");
    assert_eq!(
        outcome,
        WorkflowOutcome::Completed,
        "AFK ticket types must not stop at the gate"
    );

    let run_log = WorkflowRunLog::open(&gait_dir.join("workflow-runs"), &run_id).unwrap();
    let events = run_log.read_events().unwrap();
    let state = reconstruct_state(&events);

    assert_eq!(
        state.node_status("gate"),
        NodeStatus::Skipped,
        "gate must be skipped on the AFK arm"
    );
    assert_eq!(
        state.node_status("resolve-hitl"),
        NodeStatus::Skipped,
        "the HITL shell node must be skipped on the AFK arm"
    );
    assert_eq!(state.node_status("resolve-afk"), NodeStatus::Done);

    let afk_output = node_complete_output(&events, "resolve-afk")
        .expect("resolve-afk must have a captured shell output");
    assert!(
        afk_output.contains("grorg action_update"),
        "shell node did not echo the stand-in grorg command: {afk_output:?}"
    );
    assert!(
        !afk_output.contains("$grill.output.decision_gist"),
        "decision_gist variable was not spliced in: {afk_output:?}"
    );
}
