use std::path::Path;
use std::sync::Arc;

use gait_core::embed::{RecordingSink, SinkFrame};
use gait_core::proto::gait::log::v1::{stream_item, StreamItem, TextDelta};
use gait_core::proto::gait::workflow::v1::workflow_event;
use gait_workflow::outcome::WorkflowOutcome;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::turn_exec::{
    testing::ScriptedTurnExecutor, TurnExecutor, TurnOutcome, TurnRequest,
};
use tempfile::TempDir;

#[derive(Default)]
struct FrameTurnExecutor {
    frames: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<SinkFrame>>>,
}

#[async_trait::async_trait(?Send)]
impl TurnExecutor for FrameTurnExecutor {
    async fn run_turn(&self, _req: TurnRequest) -> anyhow::Result<TurnOutcome> {
        let frame = SinkFrame::Item(Box::new(StreamItem {
            item: Some(stream_item::Item::Delta(TextDelta {
                content: "from executor".into(),
            })),
        }));
        self.frames
            .lock()
            .unwrap()
            .as_ref()
            .expect("attach_output must be called before run_turn")
            .send(frame)
            .unwrap();
        Ok(TurnOutcome {
            output: "done".into(),
            exit: Some(0),
            ..TurnOutcome::default()
        })
    }

    fn attach_output(&self, frames: tokio::sync::mpsc::UnboundedSender<SinkFrame>) {
        *self.frames.lock().unwrap() = Some(frames);
    }
}

fn init_git_repo(dir: &Path) {
    std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
}

async fn run(
    tmp: &TempDir,
    id: &str,
    source: &str,
    force: Vec<String>,
    exec: Arc<ScriptedTurnExecutor>,
) -> anyhow::Result<WorkflowOutcome> {
    let path = tmp.path().join("wf.js");
    std::fs::write(&path, source).unwrap();
    let mut sink = gait_core::embed::CliSink::stdio(true);
    run_script(
        ScriptRunOpts {
            script_path: path,
            run_id: id.into(),
            gait_dir: tmp.path().into(),
            codebase: tmp.path().into(),
            args: vec![],
            force_rerun_nodes: force,
            force: false,
            turns: exec,
        },
        &mut sink,
        &mut gait_workflow::observer::NullObserver,
    )
    .await
}
fn events(tmp: &TempDir, id: &str) -> Vec<workflow_event::Kind> {
    WorkflowRunLog::open(&tmp.path().join("workflow-runs"), id)
        .unwrap()
        .read_events()
        .unwrap()
        .into_iter()
        .map(|e| e.kind.unwrap())
        .collect()
}
fn truncate(tmp: &TempDir, id: &str, n: usize) {
    let p = tmp.path().join("workflow-runs").join(id).join("log.jsonl");
    let lines: Vec<_> = std::fs::read_to_string(&p)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    std::fs::write(p, format!("{}\n", lines[..n].join("\n"))).unwrap();
}
fn marker_count(p: &Path) -> usize {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .count()
}

#[tokio::test]
async fn crashed_prompt_turn_refused_before_script_runs_unless_force_rerun() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("marker");
    let m = serde_json::to_string(&marker.to_string_lossy().to_string()).unwrap();
    let source = format!(
        r#"await gait.wf.config({{name:"t"}}); await gait.wf.turn({{id:"prompt", prompt:"x"}}); await gait.wf.shell({{id:"side", script:"echo x >> $M", env:{{M:{m}}}}});"#
    );
    let mut e = ScriptedTurnExecutor::new();
    e.script_text("ok");
    run(&tmp, "crashed-prompt", &source, vec![], Arc::new(e))
        .await
        .unwrap();
    let es = events(&tmp, "crashed-prompt");
    let i = es
        .iter()
        .position(|k| matches!(k, workflow_event::Kind::NodeStart(n) if n.node_id == "prompt"))
        .unwrap();
    truncate(&tmp, "crashed-prompt", i + 1);
    let before_guard = marker_count(&marker);
    let err = run(
        &tmp,
        "crashed-prompt",
        &source,
        vec![],
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("prompt") && err.contains("--force-rerun"));
    assert_eq!(marker_count(&marker), before_guard);
    let mut e = ScriptedTurnExecutor::new();
    e.script_text("ok");
    run(
        &tmp,
        "crashed-prompt",
        &source,
        vec!["prompt".into()],
        Arc::new(e),
    )
    .await
    .unwrap();
    assert_eq!(marker_count(&marker), before_guard + 1);
}

#[tokio::test]
async fn duplicate_turn_id_is_rejected() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let mut e = ScriptedTurnExecutor::new();
    e.script_text("one");
    // Keep the run's in-flight registry non-empty so both Promise.all arms
    // reach the host before the first turn completes.
    let source = r#"await gait.wf.config({name:"t"}); await Promise.all([gait.wf.shell({id:"keep-alive", script:"sleep 1"}), gait.wf.turn({id:"same", prompt:"p"}), gait.wf.turn({id:"same", prompt:"p"})]);"#;
    let err = run(&tmp, "duplicate-turn", source, vec![], Arc::new(e))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.to_lowercase().contains("duplicate") && err.contains("same"),
        "{err}"
    );
}

#[tokio::test]
async fn unannounced_gate_pauses_again_then_resolves() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source =
        r#"await gait.wf.config({name:"t"}); await gait.wf.gate({id:"review", message:"look"});"#;
    let id = "unannounced-gate";

    assert!(matches!(
        run(
            &tmp,
            id,
            source,
            vec![],
            Arc::new(ScriptedTurnExecutor::new())
        )
        .await,
        Ok(WorkflowOutcome::GatePaused)
    ));
    let all = events(&tmp, id);
    let start = all
        .iter()
        .position(|kind| {
            matches!(kind, workflow_event::Kind::NodeStart(node) if node.node_id == "review")
        })
        .unwrap();
    // Simulate interruption after NodeStart but before the runner announces
    // the pause.
    truncate(&tmp, id, start + 1);

    assert!(matches!(
        run(
            &tmp,
            id,
            source,
            vec![],
            Arc::new(ScriptedTurnExecutor::new())
        )
        .await,
        Ok(WorkflowOutcome::GatePaused)
    ));
    let after_pause = events(&tmp, id);
    assert_eq!(
        after_pause
            .iter()
            .filter(|kind| matches!(kind, workflow_event::Kind::NodeStart(node) if node.node_id == "review"))
            .count(),
        1
    );
    assert_eq!(
        after_pause
            .iter()
            .filter(|kind| matches!(kind, workflow_event::Kind::WorkflowGate(gate) if gate.node_id == "review"))
            .count(),
        1
    );

    assert!(matches!(
        run(
            &tmp,
            id,
            source,
            vec![],
            Arc::new(ScriptedTurnExecutor::new())
        )
        .await,
        Ok(WorkflowOutcome::Completed)
    ));
    let final_events = events(&tmp, id);
    assert_eq!(
        final_events
            .iter()
            .filter(|kind| matches!(kind, workflow_event::Kind::NodeComplete(node) if node.node_id == "review"))
            .count(),
        1
    );
}

#[tokio::test]
async fn record_survives_a_pause_and_is_finished_on_continue() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let command = serde_json::to_string(&format!("echo x >> '{}'", marker.display())).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{
            name: "t",
            record: {{ commands: [{{ id: "rec", command: {command}, timeout: 10000 }}] }},
        }});
        await gait.wf.gate({{ id: "g", message: "m" }});
        "#
    );
    let id = "record-gate";

    assert!(matches!(
        run(
            &tmp,
            id,
            &source,
            vec![],
            Arc::new(ScriptedTurnExecutor::new())
        )
        .await,
        Ok(WorkflowOutcome::GatePaused)
    ));
    let record_path = tmp
        .path()
        .join("workflow-runs")
        .join(id)
        .join("record.json");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(record["workflow_status"], "paused");
    assert!(!record["final_tree_hash"].as_str().unwrap().is_empty());
    assert_eq!(marker_count(&marker), 1);
}

#[tokio::test]
async fn record_of_a_failed_run_says_failed_and_continue_can_complete_it() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let command = serde_json::to_string(&format!("echo x >> '{}'", marker.display())).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{
            name: "t",
            record: {{ commands: [{{ id: "rec", command: {command}, timeout: 10000 }}] }},
        }});
        throw new Error("boom");
        "#
    );
    let id = "record-failed";

    assert!(run(
        &tmp,
        id,
        &source,
        vec![],
        Arc::new(ScriptedTurnExecutor::new())
    )
    .await
    .is_err());
    let record_path = tmp
        .path()
        .join("workflow-runs")
        .join(id)
        .join("record.json");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(record["workflow_status"], "failed");
    assert_eq!(marker_count(&marker), 1);

    // A record command is never re-run: replaying a failed run still fails
    // (the script throws again) but the record command stays at one run.
    assert!(run(
        &tmp,
        id,
        &source,
        vec![],
        Arc::new(ScriptedTurnExecutor::new())
    )
    .await
    .is_err());
    assert_eq!(marker_count(&marker), 1);
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(record["workflow_status"], "failed");
}

#[tokio::test]
async fn turn_executor_frames_reach_the_run_sink() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let path = tmp.path().join("wf.js");
    std::fs::write(
        &path,
        r#"await gait.wf.config({name:"t"}); await gait.wf.turn({id:"prompt", prompt:"x"});"#,
    )
    .unwrap();
    let mut sink = RecordingSink::default();

    let outcome = run_script(
        ScriptRunOpts {
            script_path: path,
            run_id: "frame-sink".into(),
            gait_dir: tmp.path().into(),
            codebase: tmp.path().into(),
            args: vec![],
            force_rerun_nodes: vec![],
            force: false,
            turns: Arc::new(FrameTurnExecutor::default()),
        },
        &mut sink,
        &mut gait_workflow::observer::NullObserver,
    )
    .await
    .unwrap();

    assert!(matches!(outcome, WorkflowOutcome::Completed));
    assert_eq!(sink.items.len(), 1);
    assert!(matches!(
        &sink.items[0].item,
        Some(stream_item::Item::Delta(TextDelta { content })) if content == "from executor"
    ));
}
