//! Proof-of-concept for a wayfinder-esque decision-resolution workflow
//! (docs/superpowers/specs/2026-09-05-wayfinder-grorg-workflow-design.md),
//! ported from the YAML engine to the JavaScript script engine:
//! one "ticket" resolved via prompt -> branch -> (gate) -> shell, with the
//! grorg submission step stubbed as `echo grorg ...` (no real grorg calls).
//!
//! Exercises, against the real script runner: a `gait.wf.turn` forced to
//! structured `outputSchema` JSON, a second turn continuing the first via
//! `continueSession`, a multi-way branch (a plain JS `if` on the first
//! turn's structured field) routing to a HITL or AFK arm, a `gait.wf.gate`
//! pause/resume on the HITL arm, straight-through completion on the AFK arm,
//! and shell interpolation of the two turns' structured fields via JS
//! template literals.
//!
//! Unlike the original YAML test, the model turns in every run here are
//! scripted with `ScriptedTurnExecutor` (no real model calls), so the whole
//! test runs offline: the workflow logic under test is the runner's, not the
//! model's. (The original YAML test was gated on ANTHROPIC_API_KEY and could
//! not have driven a live turn from this test binary anyway — there is no
//! GAIT_BRAIN_EXE here to hand to a real executor.)

use std::sync::Arc;

use gait_core::proto::gait::workflow::v1::workflow_event;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::WorkflowOutcome;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::state::{reconstruct_state, NodeStatus};
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;
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

/// One script run (fresh or resumed — same run_id and script path either
/// way), with the given scripted turn executor.
async fn run_js(
    tmp: &TempDir,
    run_id: &str,
    source: &str,
    exec: Arc<ScriptedTurnExecutor>,
) -> anyhow::Result<WorkflowOutcome> {
    let script_path = tmp.path().join("wf.js");
    std::fs::write(&script_path, source).unwrap();
    let mut sink = gait_core::embed::CliSink::stdio(true);
    run_script(
        ScriptRunOpts {
            script_path,
            run_id: run_id.to_string(),
            gait_dir: tmp.path().to_path_buf(),
            codebase: tmp.path().to_path_buf(),
            args: vec![],
            force_rerun_nodes: vec![],
            force: false,
            turns: exec,
        },
        &mut sink,
        &mut gait_workflow::observer::NullObserver,
    )
    .await
}

/// The scripted JSON the `grill` turn returns. `hint` pins the model's
/// `ticket_type` choice so the branch arm taken is deterministic instead of
/// left to the model's judgment (the YAML test did this via the prompt).
fn grill_json(hint: &str) -> serde_json::Value {
    serde_json::json!({
        "ticket_type": hint,
        "decision_gist": "Use an in-process LRU cache.",
        "out_of_scope": false,
    })
}

/// The scripted JSON the `domain-model` turn returns.
fn children_json() -> serde_json::Value {
    serde_json::json!({
        "new_children": ["evict policy spec", "metrics dashboard"],
    })
}

/// The workflow script for one wayfinder ticket. `hint` is baked into the
/// grill prompt (as the YAML version did) and also into the scripted grill
/// answer, so the branch is deterministic.
fn workflow_script(hint: &str) -> String {
    format!(
        r#"
        await gait.wf.config({{ name: "wayfinder-poc" }});

        const GRILL_SCHEMA = {{
            type: "object",
            required: ["ticket_type", "decision_gist", "out_of_scope"],
            properties: {{
                ticket_type: {{
                    type: "string",
                    enum: ["grilling", "prototype", "research", "task"],
                }},
                decision_gist: {{ type: "string" }},
                out_of_scope: {{ type: "boolean" }},
            }},
        }};
        const CHILDREN_SCHEMA = {{
            type: "object",
            required: ["new_children"],
            properties: {{
                new_children: {{ type: "array", items: {{ type: "string" }} }},
            }},
        }};

        const grill = await gait.wf.turn({{
            id: "grill",
            prompt: `
You are resolving one decision ticket from a work-planning map,
wayfinder-style. The decision to grill: "Should the new caching layer
use Redis or an in-process LRU cache?" Pick an answer and write one
sentence explaining it as decision_gist.
Set ticket_type to exactly "{hint}" for this exercise (ignore whether
that's the type you'd naturally pick). Set out_of_scope to false.
`,
            outputSchema: GRILL_SCHEMA,
        }});

        const domainModel = await gait.wf.turn({{
            id: "domain-model",
            prompt: `
Given the decision you just made, list 0 to 2 short new child-ticket
titles it spawns (an empty list is fine).
`,
            // continueSession takes a SESSION ID, not a step id: use the
            // first turn's reported session id.
            continueSession: grill.sessionId,
            outputSchema: CHILDREN_SCHEMA,
        }});

        const gist = grill.json.decision_gist;
        const children = domainModel.json.new_children.join(" ");
        const submit = (id) => gait.wf.shell({{
            id,
            script: `echo grorg action_update decision="${{gist}}" children="${{children}}"`,
        }});

        if (
            grill.json.ticket_type === "grilling" ||
            grill.json.ticket_type === "prototype"
        ) {{
            await gait.wf.gate({{
                id: "gate",
                message:
                    "Review the grilled decision, then run: gait workflow continue $WORKFLOW_ID",
            }});
            await submit("resolve-hitl");
        }} else {{
            await submit("resolve-afk");
        }}
    "#,
        hint = hint,
    )
}

fn node_complete_output(
    events: &[gait_core::proto::gait::workflow::v1::WorkflowEvent],
    node_id: &str,
) -> Option<String> {
    events.iter().find_map(|e| match &e.kind {
        Some(workflow_event::Kind::NodeComplete(nc)) if nc.node_id == node_id => {
            Some(nc.output.clone())
        }
        _ => None,
    })
}

fn node_start_session_id(
    events: &[gait_core::proto::gait::workflow::v1::WorkflowEvent],
    node_id: &str,
) -> Option<String> {
    events.iter().find_map(|e| match &e.kind {
        Some(workflow_event::Kind::NodeStart(ns)) if ns.node_id == node_id => {
            Some(ns.session_id.clone())
        }
        _ => None,
    })
}

/// Two turns' worth of scripted outcomes in order: the grill answer (with a
/// fake session id, so `grill.sessionId` and `continueSession` chaining work)
/// then the domain-model answer. Each answer is scripted with `script_json`
/// (and a follow-up `.session_id(...)` where the turn must report one); a
/// Json outcome feeds `turn.json` directly, exactly as a real schema turn's
/// stdout would be parsed by the host.
fn scripted_turns(hint: &str) -> ScriptedTurnExecutor {
    let mut exec = ScriptedTurnExecutor::new();
    exec.script_json(grill_json(hint)).session_id("sess-grill");
    exec.script_json(children_json());
    exec
}

#[tokio::test]
async fn hitl_arm_gates_then_resumes_with_spliced_output() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git(&gait_dir);

    let source = workflow_script("grilling");
    let run_id = "wayfinder-poc-hitl-001";

    // --- First run: must pause at the gate, not complete.
    let first = run_js(&tmp, run_id, &source, Arc::new(scripted_turns("grilling")))
        .await
        .expect("first run should pause cleanly at the gate");
    assert_eq!(first, WorkflowOutcome::GatePaused);

    let run_log = WorkflowRunLog::open(&gait_dir.join("workflow-runs"), run_id).unwrap();
    let events_before_resume = run_log.read_events().unwrap();
    let state_before_resume = reconstruct_state(&events_before_resume);
    assert_eq!(
        state_before_resume.node_status("resolve-hitl"),
        NodeStatus::Pending,
        "resolve-hitl must not have run before the gate is cleared"
    );

    // --- Resume: same run_id, same script -- what `gait workflow continue` does.
    // The executor stays EMPTY on purpose: every already-completed turn is
    // replayed from the run log, so any executor call in this run would mean
    // a spurious turn ran and fails the test ("no scripted outcome left for
    // the request").
    let second = run_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new()))
        .await
        .expect("resume should complete the workflow");
    assert_eq!(second, WorkflowOutcome::Completed);

    let events = run_log.read_events().unwrap();
    let state = reconstruct_state(&events);

    assert_eq!(state.node_status("grill"), NodeStatus::Done);
    assert_eq!(state.node_status("domain-model"), NodeStatus::Done);
    assert_eq!(state.node_status("gate"), NodeStatus::Done);
    assert_eq!(
        state.node_status("resolve-hitl"),
        NodeStatus::Done,
        "the HITL arm must have actually run post-resume"
    );
    // The AFK arm is a plain `else` branch in the script, so it never runs at
    // all: no NodeStart, hence no Done. (The YAML engine logged an explicit
    // NodeSkip for the untaken arm; the script engine has no such event.)
    assert_eq!(
        state.node_status("resolve-afk"),
        NodeStatus::Pending,
        "the AFK arm must not have run -- the branch selected the other arm"
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
        "new_children variable (from the continued-session turn) was not spliced in: {hitl_output:?}"
    );
    // The spliced values must be the scripted structured fields themselves.
    assert!(
        hitl_output.contains("Use an in-process LRU cache."),
        "the scripted decision_gist must appear in the shell output: {hitl_output:?}"
    );
    assert!(
        hitl_output.contains("evict policy spec") && hitl_output.contains("metrics dashboard"),
        "the scripted new_children must appear in the shell output: {hitl_output:?}"
    );
}

#[tokio::test]
async fn afk_arm_skips_gate_and_completes_in_one_run() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git(&gait_dir);

    let source = workflow_script("research");
    let run_id = "wayfinder-poc-afk-001";

    let outcome = run_js(&tmp, run_id, &source, Arc::new(scripted_turns("research")))
        .await
        .expect("AFK arm should complete in a single run, no gate to pause at");
    assert_eq!(
        outcome,
        WorkflowOutcome::Completed,
        "AFK ticket types must not stop at the gate"
    );

    let run_log = WorkflowRunLog::open(&gait_dir.join("workflow-runs"), run_id).unwrap();
    let events = run_log.read_events().unwrap();
    let state = reconstruct_state(&events);

    // The gate and the HITL arm live on the untaken `else`-complement branch
    // of a plain JS `if`, so they never run at all: no NodeStart, hence no
    // Done. (The YAML engine logged an explicit NodeSkip; the script engine
    // has no such event, so "skipped" is asserted as "never started".)
    assert_eq!(
        state.node_status("gate"),
        NodeStatus::Pending,
        "gate must be skipped on the AFK arm"
    );
    assert_eq!(
        state.node_status("resolve-hitl"),
        NodeStatus::Pending,
        "the HITL shell node must be skipped on the AFK arm"
    );
    assert_eq!(state.node_status("resolve-afk"), NodeStatus::Done);

    // The NodeStart event for the continuation turn must carry the grill
    // turn's session id: `continueSession` was given a SESSION ID (via
    // `grill.sessionId`), not a step id, and the host passes it straight
    // through as the step's session id.
    assert_eq!(
        node_start_session_id(&events, "domain-model"),
        Some("sess-grill".to_string()),
        "domain-model must continue the grill turn's session"
    );

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
    assert!(
        afk_output.contains("Use an in-process LRU cache."),
        "the scripted decision_gist must appear in the shell output: {afk_output:?}"
    );
}
