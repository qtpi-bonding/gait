//! The one blessed degenerate-workflow API (spec-06 §1). Both the REPL and (in
//! Stage 3) the CLI construct their single `PromptNode` run here — nowhere else.
use crate::observer::WorkflowObserver;
use crate::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use crate::TurnSink;
use gait_core::embed::TurnSource;
use gait_core::proto::gait::workflow::v1::{node, Node, PromptNode, Workflow};
use std::path::PathBuf;

pub struct SinglePromptOpts {
    /// Run identity. REPL passes `chat-<session>`; CLI mints via `convert::mint_run_id` (Stage 3).
    pub run_id: String,
    /// `""` = fresh session; else attach (spec-04 semantics).
    pub continue_session: String,
    /// `None` = open/interactive node (drive with `input`); `Some` = literal one-shot.
    pub prompt: Option<String>,
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub args: Vec<String>,
}

/// Build the one-node `PromptNode` `Workflow` and drive it through `run_workflow`.
pub async fn single_prompt_run(
    opts: SinglePromptOpts,
    sink: &mut dyn TurnSink,
    observer: &mut dyn WorkflowObserver,
    input: Option<Box<dyn TurnSource>>,
) -> anyhow::Result<WorkflowOutcome> {
    let SinglePromptOpts {
        run_id,
        continue_session,
        prompt,
        gait_dir,
        codebase,
        args,
    } = opts;
    let node = Node {
        id: "chat".into(),
        body: Some(node::Body::Prompt(PromptNode {
            prompt: prompt.unwrap_or_default(),
            continue_session,
            codebase: None,
        })),
        ..Default::default()
    };
    let workflow_def = Workflow {
        name: "chat".into(),
        nodes: vec![node],
        ..Default::default()
    };
    run_workflow(
        WorkflowRunOpts {
            workflow_def,
            yaml_path: String::new(),
            run_id,
            gait_dir,
            codebase,
            args,
        },
        sink,
        observer,
        input,
    )
    .await
}
