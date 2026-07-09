//! The one blessed degenerate-workflow API (spec-06 §1). Both the REPL and (in
//! Stage 3) the CLI construct their single `PromptNode` run here — nowhere else.
use crate::observer::WorkflowObserver;
use crate::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use crate::TurnSink;
use gait_core::embed::TurnSource;
use gait_core::proto::gait::workflow::v1::{node, Node, PromptNode, Workflow};
use std::path::PathBuf;

/// CLI-only degenerate-run overrides threaded through `SinglePromptOpts` (spec-06
/// §3 Task 3.2, the C1 fix). `exec_prompt_node` normally re-resolves its model/
/// allow/agent from disk config; a real `--model`/`--allow`/`--deny`/`--agent`/
/// `--max-iter`/result-schema passed to `gait run` must NOT be silently dropped
/// just because the run is routed through the workflow layer now. `None`/empty
/// fields fall back to disk config (the REPL's degenerate node — no override —
/// is unaffected).
#[derive(Default, Clone)]
pub struct ProfileOverride {
    pub model: Option<String>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub agent: Option<String>,
    pub max_iter: Option<usize>,
    pub result_schema: Option<serde_json::Value>,
    /// CLI `--auto`: `Some(true)` forces auto-approve on for the degenerate CLI
    /// node; `None` falls back to the node/workflow default. Never set on real
    /// workflow nodes (they carry no ProfileOverride).
    pub auto: Option<bool>,
}

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
    /// `None` = node resolves sandbox from disk config (REPL default). `Some` =
    /// the CLI's resolved `--sandbox`/`--no-sandbox`/config value.
    pub sandbox_on: Option<bool>,
    /// Cascade group handle for a spawned subagent brain (CLI enrollment).
    pub cascade_group_arg: Option<String>,
    /// The CLI's process-lifetime cascade kill handle, threaded into `run_loop`.
    pub cascade: Option<gait_core::embed::CascadeKillHandle>,
    /// The CLI's resolved profile (model/allow/deny/agent/max_iter/schema).
    pub profile: Option<ProfileOverride>,
    /// Optional editor-buffer delegate injected by a workflow-level consumer
    /// (e.g. a future ACP adapter). `None` for REPL/CLI/workflow nodes.
    pub fs_delegate: Option<std::sync::Arc<dyn gait_core::embed::FsDelegate>>,
    /// Launcher-set override for the in-tree safety guard (design §7), sourced
    /// from the CLI's `--allow-intree` flag. `false` for REPL/ACP.
    pub allow_intree: bool,
}

impl Default for SinglePromptOpts {
    fn default() -> Self {
        Self {
            run_id: String::new(),
            continue_session: String::new(),
            prompt: None,
            gait_dir: PathBuf::new(),
            codebase: PathBuf::new(),
            args: Vec::new(),
            sandbox_on: None,
            cascade_group_arg: None,
            cascade: None,
            profile: None,
            fs_delegate: None,
            allow_intree: false,
        }
    }
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
        sandbox_on,
        cascade_group_arg,
        cascade,
        profile,
        fs_delegate,
        allow_intree,
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
            sandbox_on,
            cascade_group_arg,
            cascade,
            profile,
            fs_delegate,
            allow_intree,
        },
        sink,
        observer,
        input,
    )
    .await
}
