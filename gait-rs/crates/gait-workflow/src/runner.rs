use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use gait_core::embed::{TurnSink, TurnSource};
use gait_core::proto::gait::log::v1::StreamItem;
use gait_core::proto::gait::workflow::v1::{node, Node, Workflow};
use gait_core::Recorder;

use crate::exec_branch::resolve_branch;
use crate::exec_gate::exec_gate_node;
use crate::exec_prompt::exec_prompt_node;
use crate::exec_shell::{exec_shell_node, NodeContext};
use crate::hooks::run_hook;
use crate::observer::WorkflowObserver;
use crate::run_log::WorkflowRunLog;
use crate::schema::GATE_PAUSED_SENTINEL;
use crate::state::{reconstruct_state, NodeStatus, WorkflowState};
use crate::vars::{substitute, NodeOutput, VarContext, NODE_OPEN_SENTINEL};

/// Inputs for `run_workflow`.
///
/// The provider is no longer passed in: each prompt node now resolves its own
/// `ResolvedProfile` (provider + tools + MCP) via `build_workflow_node_profile`
/// inside `exec_prompt_node`, driven through `run_loop`. This keeps workflow
/// nodes independent of any provider the CLI/REPL constructed for the parent
/// session.
pub struct WorkflowRunOpts {
    pub workflow_def: Workflow,
    pub yaml_path: String,
    pub run_id: String,
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub args: Vec<String>,
}

type NodeFuture =
    Pin<Box<dyn std::future::Future<Output = anyhow::Result<(String, NodeTaskOutcome)>>>>;

/// `tasks.next()` only yields `None` when the pool is empty, which the `loop`
/// guard already excludes; this makes that unreachable explicit.
fn unreachable_empty_tasks() -> (String, NodeTaskOutcome) {
    unreachable!("tasks.next() returned None while pool was non-empty")
}

#[derive(Debug)]
enum NodeTaskOutcome {
    Done(NodeOutput),
    GatePaused(NodeOutput), // carries gate message in text field
    Open,                   // absent-prompt node, no input -> run exits, node left open
    Error(String),          // abort after retries exhausted
    ErrorContinue,          // on_error: continue — treat as skipped
    ErrorGoto(String),      // on_error: goto <node_id>
}

struct NodeRunParams {
    node: Node,
    workflow_id: String,
    session_id: String,
    node_session: String,
    gait_dir: PathBuf,
    codebase: PathBuf,
    run_worktree: PathBuf,
    wf: Arc<Workflow>,
    vars: VarContext,
    workflow_tx: tokio::sync::mpsc::UnboundedSender<StreamItem>,
}

/// Derive the node type name from its proto body oneof.
fn node_type_name(node: &Node) -> &'static str {
    match node.body.as_ref() {
        Some(node::Body::Shell(_)) => "shell",
        Some(node::Body::Prompt(_)) => "prompt",
        Some(node::Body::Branch(_)) => "branch",
        Some(node::Body::Gate(_)) => "gate",
        None => "unknown",
    }
}

/// Run-workspace predicate (spec-06 §2). The run branch/worktree/lock exist only
/// for a non-degenerate graph: the run worktree is the shared cwd for **shell**
/// nodes, and the run lock guards concurrent executors of a multi-node run. A lone
/// prompt node needs neither — its own session worktree + SessionLock suffice.
fn needs_run_workspace(wf: &Workflow) -> bool {
    wf.nodes.len() > 1
        || wf
            .nodes
            .iter()
            .any(|n| matches!(n.body.as_ref(), Some(node::Body::Shell(_))))
}

/// The effective gait session id for a node's turn (item 13b/13c).
/// - prompt node, empty `continue_session` → a **fresh** session (own worktree+lock+log,
///   zero context carryover);
/// - prompt node, non-empty `continue_session` → attach to that session id;
/// - shell/branch/gate → `""` (no session — matches workflow.proto:94).
fn node_session_id(node: &Node) -> String {
    match node.body.as_ref() {
        Some(node::Body::Prompt(p)) if p.continue_session.is_empty() => {
            gait_core::session::new_session_id()
        }
        Some(node::Body::Prompt(p)) => p.continue_session.clone(),
        _ => String::new(),
    }
}

/// Idempotent per-node session resolution. Reuses a session already recorded for
/// this node (resume via `reconstruct_state`, or goto/branch re-entry within the
/// live run); mints a fresh one only on the node's FIRST enqueue. This is what
/// keeps an open/interactive node's transcript intact across pause/resume.
fn resolve_node_session(state: &mut WorkflowState, node: &Node) -> String {
    if let Some(prev) = state.node_sessions.get(&node.id) {
        return prev.clone();
    }
    let s = node_session_id(node);
    if !s.is_empty() {
        state.node_sessions.insert(node.id.clone(), s.clone());
    }
    s
}

/// Returns *automated* ready node indices only — deps satisfied, not completed,
/// not in-flight, not excluded (`exclude` skips goto-only targets — they should
/// only be enqueued via ErrorGoto), and NOT interactive. Interactive
/// (absent-prompt) nodes never enter the `FuturesUnordered` pool; they are
/// handled exclusively by `ready_interactive_nodes` + `handle_open_node` once
/// the automated frontier (this function's output) drains to empty.
fn compute_ready_nodes(
    nodes: &[Node],
    completed: &HashSet<String>,
    in_flight: &HashSet<String>,
    exclude: &HashSet<String>,
) -> Vec<usize> {
    nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            !completed.contains(&n.id)
                && !in_flight.contains(&n.id)
                && !exclude.contains(&n.id)
                && !is_interactive_node(n)
                && n.depends_on.iter().all(|dep| completed.contains(dep))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Node indices that are ready (deps satisfied, not completed, not in-flight)
/// **and** interactive (absent-prompt). These are never enqueued into the pool;
/// they are suspension points handled by `handle_open_node` after the automated
/// frontier drains. Mirrors `compute_ready_nodes`' predicate but keeps only
/// interactive nodes.
fn ready_interactive_nodes(
    nodes: &[Node],
    completed: &HashSet<String>,
    in_flight: &HashSet<String>,
) -> Vec<usize> {
    nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            !completed.contains(&n.id)
                && !in_flight.contains(&n.id)
                && is_interactive_node(n)
                && n.depends_on.iter().all(|dep| completed.contains(dep))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Returns the set of node IDs that are exclusively reachable via `on_error.goto`
/// and have no `depends_on` entries (i.e., they would otherwise be picked up as
/// initial ready nodes but should only be enqueued on error).
fn goto_only_nodes(nodes: &[Node]) -> HashSet<String> {
    // Collect all node IDs referenced as on_error.goto targets.
    let goto_targets: HashSet<String> = nodes
        .iter()
        .filter_map(|n| {
            n.on_error
                .as_ref()
                .and_then(|oe| oe.goto.as_ref())
                .filter(|g| g.as_str() != "abort" && g.as_str() != "continue")
                .cloned()
        })
        .collect();
    // A node is "goto-only" if it appears as a goto target and has no depends_on
    // (nodes with depends_on are already blocked from the initial ready set).
    goto_targets
        .into_iter()
        .filter(|id| {
            nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.depends_on.is_empty())
                .unwrap_or(false)
        })
        .collect()
}

/// Sync the completed set with state — any Done/Skipped/Error/Paused node is "complete"
/// for the purpose of unblocking dependents.
fn sync_completed(completed: &mut HashSet<String>, state: &WorkflowState, nodes: &[Node]) {
    for n in nodes {
        if matches!(
            state.node_status(&n.id),
            NodeStatus::Done | NodeStatus::Skipped | NodeStatus::Error(_) | NodeStatus::Paused
        ) {
            completed.insert(n.id.clone());
        }
    }
}

/// Backoff delay for retry attempt N (1-based).
fn backoff_delay_ms(attempt: u32, base_ms: u64, backoff: &str) -> u64 {
    match backoff {
        "exponential" => base_ms.saturating_mul(2u64.saturating_pow(attempt - 1)),
        _ => base_ms,
    }
}

/// Execute a single node with retries. Returns the node_id and outcome.
/// Interactive (absent-prompt) nodes pass `None` for input — they always return
/// `NodeTaskOutcome::Open` which the scheduler handles outside this function.
async fn run_single_node(params: NodeRunParams) -> anyhow::Result<(String, NodeTaskOutcome)> {
    let node_id = params.node.id.clone();
    let on_error = params.node.on_error.clone().unwrap_or_default();
    let max_attempts = on_error.retry_times as usize + 1;

    for attempt in 1..=max_attempts {
        if attempt > 1 {
            let delay = backoff_delay_ms(attempt as u32, on_error.delay, &on_error.backoff);
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }

        let ctx = NodeContext {
            workflow_id: params.workflow_id.as_str(),
            session_id: params.session_id.as_str(),
            node_session: params.node_session.as_str(),
            gait_dir: params.gait_dir.as_path(),
            codebase: params.codebase.as_path(),
            run_worktree: params.run_worktree.as_path(),
            vars: params.vars.clone(),
            workflow_tx: &params.workflow_tx,
        };

        // Interactive/absent-prompt nodes are run outside FuturesUnordered with
        // the owned input source. Inside the task pool, always pass None.
        match execute_node(&params.node, &ctx, &params.wf, None).await {
            Ok(out) => {
                if out.routed_to.as_deref() == Some(GATE_PAUSED_SENTINEL) {
                    return Ok((node_id, NodeTaskOutcome::GatePaused(out)));
                }
                if out.routed_to.as_deref() == Some(NODE_OPEN_SENTINEL) {
                    return Ok((node_id, NodeTaskOutcome::Open));
                }
                return Ok((node_id, NodeTaskOutcome::Done(out)));
            }
            Err(e) => {
                if attempt < max_attempts {
                    continue;
                }
                // All retries exhausted — route by on_error
                if on_error.goto.as_deref() == Some("continue") {
                    return Ok((node_id, NodeTaskOutcome::ErrorContinue));
                }
                if let Some(goto_id) = on_error
                    .goto
                    .as_deref()
                    .filter(|&g| g != "abort" && g != "continue")
                {
                    return Ok((node_id, NodeTaskOutcome::ErrorGoto(goto_id.to_string())));
                }
                return Ok((node_id, NodeTaskOutcome::Error(e.to_string())));
            }
        }
    }
    unreachable!()
}

/// Prepare and push a node onto the ready queue.
/// Calls before-hooks, emits node_start, sets Running status, builds VarContext.
#[allow(clippy::too_many_arguments)]
async fn enqueue_node_task(
    node: Node,
    wf: &Arc<Workflow>,
    workflow_id: &str,
    session_id: &str,
    gait_dir: &Path,
    codebase: &Path,
    run_worktree: &Path,
    args_str: &str,
    state: &mut WorkflowState,
    in_flight: &mut HashSet<String>,
    tasks: &mut FuturesUnordered<NodeFuture>,
    rec: &Recorder,
    run_log: &WorkflowRunLog,
    observer: &mut dyn WorkflowObserver,
) -> anyhow::Result<()> {
    let node_id = node.id.clone();
    let env = make_env_vars(
        workflow_id,
        session_id,
        gait_dir,
        codebase,
        args_str,
        Some(&node_id),
        state,
    );

    if let Some(hooks) = &node.hooks {
        if let Some(cmd) = &hooks.before {
            let (exit, _) = run_hook(cmd, &env).await;
            observer.on_event(&run_log.hook_run(&node_id, "before", cmd, exit)?);
        }
    }

    let ntype = node_type_name(&node);
    let node_session = resolve_node_session(state, &node);
    observer.on_event(&run_log.node_start(&node_id, ntype, 1, &node_session)?);
    state.set_status(&node_id, NodeStatus::Running);

    let vars = VarContext {
        workflow_id: workflow_id.to_string(),
        session_id: session_id.to_string(),
        gait_dir: gait_dir.to_path_buf(),
        codebase: codebase.to_path_buf(),
        args: args_str.to_string(),
        node_id: node_id.clone(),
        node_outputs: state.node_outputs.clone(),
        node_exit: None,
        failed_node: state.failed_node.clone(),
    };

    in_flight.insert(node_id);
    tasks.push(Box::pin(run_single_node(NodeRunParams {
        node,
        workflow_id: workflow_id.to_string(),
        session_id: session_id.to_string(),
        node_session,
        gait_dir: gait_dir.to_path_buf(),
        codebase: codebase.to_path_buf(),
        run_worktree: run_worktree.to_path_buf(),
        wf: wf.clone(),
        vars,
        workflow_tx: rec.sender().clone(),
    })));

    Ok(())
}

/// Outcome of a completed `run_workflow` call.
#[derive(Debug, PartialEq)]
pub enum WorkflowOutcome {
    /// All nodes completed successfully.
    Completed,
    /// Workflow paused at a gate node, awaiting external input to resume.
    GatePaused,
    /// Workflow paused at an open (absent-prompt) node awaiting interactive input.
    /// The node was started (`NodeStart` recorded) but not completed. Resume with
    /// an input source to continue from this point.
    NodeOpen,
}

/// Returns `true` if the node is an absent-prompt (interactive) node that
/// should be driven with a live `TurnSource` rather than queued in the
/// `FuturesUnordered` pool.
fn is_interactive_node(node: &gait_core::proto::gait::workflow::v1::Node) -> bool {
    use gait_core::proto::gait::workflow::v1::node;
    matches!(node.body.as_ref(), Some(node::Body::Prompt(p)) if p.prompt.is_empty())
}

/// How the runner resumes at an open (interactive) node. Chosen by whether a live
/// input source is in hand: REPL → `ResumeInline`; CLI/headless → `SuspendAndExit`.
enum OpenResumption {
    /// No live input: settle progress and let the caller return `NodeOpen`; the
    /// process exits and `gait workflow continue` re-enters. (Advancing *past* an
    /// open node — supplying the response, marking Done — is a future finalize
    /// model, not item 8; item 8 only relocates the suspension point.)
    SuspendAndExit,
    /// Live source: drive the node inline off the source, rendering through the
    /// runner's sink, then let the caller return `NodeOpen` (behavior-preserving;
    /// a future finalize model may map the turn result to completion here).
    ResumeInline(Box<dyn TurnSource>),
}

/// Handle a single open interactive node. Resolves the node's effective session
/// (reuse-or-mint via `resolve_node_session`), records `node_start` **with that
/// session**, then applies the resumption strategy. No completion is recorded —
/// the node is deliberately left `Running` so `reconstruct_state`'s `open_node()`
/// keeps returning it; the caller returns `WorkflowOutcome::NodeOpen` after this.
///
/// `session_id` is the *run's* bookkeeping session id (env/hooks); the node's
/// own turn runs in `node_session` from `resolve_node_session`.
#[allow(clippy::too_many_arguments)]
async fn handle_open_node(
    node: &Node,
    wf: &Arc<Workflow>,
    workflow_id: &str,
    session_id: &str,
    gait_dir: &Path,
    codebase: &Path,
    run_worktree: &Path,
    args_str: &str,
    state: &mut WorkflowState,
    rec: &Recorder,
    run_log: &WorkflowRunLog,
    observer: &mut dyn WorkflowObserver,
    sink: &mut dyn TurnSink,
    strategy: OpenResumption,
) -> anyhow::Result<()> {
    let node_id = node.id.clone();
    // Reuse a session already recorded for this node (resume/re-entry); mint
    // only on first enqueue — never the bare `node_session_id` minter here, or
    // a re-entered open node would orphan its own transcript on every resume.
    let node_session = resolve_node_session(state, node);
    observer.on_event(&run_log.node_start(&node_id, "prompt", 1, &node_session)?);
    state.set_status(&node_id, NodeStatus::Running);

    if let OpenResumption::ResumeInline(live) = strategy {
        let vars = VarContext {
            workflow_id: workflow_id.to_string(),
            session_id: session_id.to_string(),
            gait_dir: gait_dir.to_path_buf(),
            codebase: codebase.to_path_buf(),
            args: args_str.to_string(),
            node_id: node_id.clone(),
            node_outputs: state.node_outputs.clone(),
            node_exit: None,
            failed_node: state.failed_node.clone(),
        };
        let ctx = crate::exec_shell::NodeContext {
            workflow_id,
            session_id,
            node_session: &node_session,
            gait_dir,
            codebase,
            run_worktree,
            vars,
            workflow_tx: rec.sender(),
        };
        let p = match node.body.as_ref() {
            Some(node::Body::Prompt(p)) => p,
            _ => unreachable!("handle_open_node called on non-prompt node"),
        };
        let _open =
            crate::exec_prompt::exec_prompt_node(node, p, &ctx, wf, Some(live), Some(sink)).await?;
    }

    Ok(())
}

pub async fn run_workflow(
    opts: WorkflowRunOpts,
    sink: &mut dyn TurnSink,
    observer: &mut dyn WorkflowObserver,
    input: Option<Box<dyn TurnSource>>,
) -> anyhow::Result<WorkflowOutcome> {
    let WorkflowRunOpts {
        workflow_def: wf_owned,
        yaml_path,
        run_id,
        gait_dir,
        codebase,
        args,
    } = opts;
    // The run id doubles as the SESSION_ID env value nodes see and as the default
    // workflow_id; it is NOT a git session — the run owns its own branch/worktree
    // (`run/<run_id>` + `workflow-runs/<run_id>/worktree/`) below. There is no
    // `sessions/<run_id>/` bookkeeping session anymore: sessions do not know
    // workflows exist (plan item 3 / I1), and prompt nodes mint their own fresh
    // per-node sessions.
    let session_id = run_id.clone();
    let wf = Arc::new(wf_owned);

    let repos = gait_core::git::Repos::production();

    let args_str = args.join(" ");
    let workflow_runs_dir = gait_dir.join("workflow-runs");
    // Open-vs-create is keyed on the run log's OWN existence — the only correct
    // predicate for resume (`gait workflow continue <run_id>` re-invokes this).
    let run_log_path = workflow_runs_dir.join(&run_id).join("log.jsonl");
    let run_log = if run_log_path.exists() {
        WorkflowRunLog::open(&workflow_runs_dir, &run_id)?
    } else {
        WorkflowRunLog::create(&workflow_runs_dir, &run_id)?
    };

    // Lazy run infrastructure (spec-06 §2): run_id + run log are always written
    // (above). The run dir lock + branch + worktree exist only for a non-degenerate
    // graph; a degenerate single-prompt run relies on its node session's own
    // worktree + SessionLock.
    let run_dir = workflow_runs_dir.join(&run_id);
    let run_worktree = run_dir.join("worktree");
    let _run_lock = if needs_run_workspace(&wf) {
        std::fs::create_dir_all(&run_dir)?;
        let lock = gait_core::turn::SessionLock::acquire(&run_dir)?;
        if !run_worktree.join(".git").exists() {
            repos.traces.create_branch_worktree(
                &gait_dir,
                "main",
                &format!("run/{run_id}"),
                &run_worktree,
            )?;
        }
        Some(lock)
    } else {
        None
    };

    let wf_events = run_log.read_events().unwrap_or_default();
    let mut state = reconstruct_state(&wf_events);

    let workflow_id = if state.workflow_id.is_empty() {
        session_id.clone()
    } else {
        state.workflow_id.clone()
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();
    // The run-level recorder is used only for its `.sender()` (StreamItem fan-out
    // to the sink); it never `.record()`s a session transcript, so it needs no
    // log path. Prompt nodes construct their own session recorders internally.
    let rec = Recorder::new(std::path::PathBuf::new(), tx);

    // Interactive (absent-prompt) nodes must be driven with the owned live
    // source outside the FuturesUnordered pool (which requires Send).  We
    // consume `input` the first time we encounter such a node; subsequent
    // interactive nodes (if any) fall through to NodeOpen as before.
    let mut live_input: Option<Box<dyn TurnSource>> = input;

    let run_result: anyhow::Result<WorkflowOutcome> = async {
        if wf_events.is_empty() {
            emit_workflow_start(
                &repos,
                &wf,
                &workflow_id,
                &yaml_path,
                &args,
                &args_str,
                &gait_dir,
                &session_id,
                &codebase,
                &state,
                &run_log,
                observer,
            )
            .await?;
        }

        if let Some(gate_id) = state.gate_paused.take() {
            state.set_status(&gate_id, NodeStatus::Done);
        }

        let mut completed: HashSet<String> = HashSet::new();
        let mut in_flight: HashSet<String> = HashSet::new();
        let mut tasks: FuturesUnordered<NodeFuture> = FuturesUnordered::new();
        let mut goto_hops: HashMap<String, u32> = HashMap::new();
        const MAX_GOTO_HOPS: u32 = 1000;

        // Seed completed from prior (resumed) run
        sync_completed(&mut completed, &state, &wf.nodes);

        // Compute nodes that are exclusively reachable via on_error.goto — these
        // must not be enqueued at startup; they are only triggered on error.
        let goto_only = goto_only_nodes(&wf.nodes);

        // Enqueue all initially ready (automated) nodes. Interactive nodes are
        // excluded from `compute_ready_nodes` and handled after the pool drains
        // (see the `tasks.is_empty()` branch below via `handle_open_node`).
        let initially_ready = compute_ready_nodes(&wf.nodes, &completed, &in_flight, &goto_only);
        for idx in initially_ready {
            let node = wf.nodes[idx].clone();
            enqueue_node_task(
                node,
                &wf,
                &workflow_id,
                &session_id,
                &gait_dir,
                &codebase,
                &run_worktree,
                &args_str,
                &mut state,
                &mut in_flight,
                &mut tasks,
                &rec,
                &run_log,
                observer,
            )
            .await?;
        }

        loop {
            if tasks.is_empty() {
                // Automated frontier exhausted. Suspend at the first ready
                // interactive node, if any; otherwise the workflow is complete.
                let open = ready_interactive_nodes(&wf.nodes, &completed, &in_flight);
                let Some(&idx) = open.first() else {
                    break;
                };
                let node = wf.nodes[idx].clone();
                let strategy = match live_input.take() {
                    Some(live) => OpenResumption::ResumeInline(live),
                    None => OpenResumption::SuspendAndExit,
                };
                handle_open_node(
                    &node,
                    &wf,
                    &workflow_id,
                    &session_id,
                    &gait_dir,
                    &codebase,
                    &run_worktree,
                    &args_str,
                    &mut state,
                    &rec,
                    &run_log,
                    observer,
                    sink,
                    strategy,
                )
                .await?;
                return Ok(WorkflowOutcome::NodeOpen);
            }

            // Drain stream items into the sink AS they arrive (not just when a
            // node finishes), so fan-out output renders live. `select!` re-polls
            // `tasks.next()` each iteration; FuturesUnordered::next is cancel-safe.
            let (node_id, outcome) = loop {
                tokio::select! {
                    biased;
                    Some(item) = rx.recv() => { sink.item(&item); }
                    res = tasks.next() => {
                        match res {
                            Some(Ok(r)) => break r,
                            Some(Err(e)) => return Err(e),
                            None => break unreachable_empty_tasks(),
                        }
                    }
                }
            };
            // Flush anything buffered between the winning task completing and now.
            while let Ok(item) = rx.try_recv() {
                sink.item(&item);
            }
            in_flight.remove(&node_id);

            let node = wf
                .nodes
                .iter()
                .find(|n| n.id == node_id)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("completed node '{}' not found in workflow", node_id)
                })?;
            let env = make_env_vars(
                &workflow_id,
                &session_id,
                &gait_dir,
                &codebase,
                &args_str,
                Some(&node_id),
                &state,
            );

            match outcome {
                NodeTaskOutcome::Open => unreachable!(
                    "interactive nodes are excluded from the pool; open handled by handle_open_node"
                ),

                NodeTaskOutcome::Done(ref out) => {
                    observer.on_event(&run_log.node_complete(
                        &node_id,
                        &out.session_id,
                        &out.text,
                        out.exit_code,
                        &out.json.as_ref().map(|v| v.to_string()).unwrap_or_default(),
                        out.routed_to.as_deref().unwrap_or(""),
                    )?);
                    state.set_status(&node_id, NodeStatus::Done);
                    state.node_outputs.insert(node_id.clone(), out.clone());
                    completed.insert(node_id.clone());

                    // After hooks
                    if let Some(hooks) = &node.hooks {
                        if let Some(cmd) = &hooks.after {
                            let (exit, _) = run_hook(cmd, &env).await;
                            observer.on_event(&run_log.hook_run(&node_id, "after", cmd, exit)?);
                        }
                    }

                    // Branch routing: mark non-taken paths as skipped, enqueue taken path
                    if let Some(node::Body::Branch(_)) = node.body.as_ref() {
                        if let Some(routed_to) = &out.routed_to {
                            propagate_branch_skips(
                                &node, routed_to, &mut state, &run_log, observer,
                            )?;
                            sync_completed(&mut completed, &state, &wf.nodes);
                            // Enqueue the branch target (supports cycles: re-enqueue even if completed)
                            if let Some(target_idx) =
                                wf.nodes.iter().position(|n| &n.id == routed_to)
                            {
                                if !in_flight.contains(routed_to.as_str()) {
                                    let target = wf.nodes[target_idx].clone();
                                    enqueue_node_task(
                                        target,
                                        &wf,
                                        &workflow_id,
                                        &session_id,
                                        &gait_dir,
                                        &codebase,
                                        &run_worktree,
                                        &args_str,
                                        &mut state,
                                        &mut in_flight,
                                        &mut tasks,
                                        &rec,
                                        &run_log,
                                        observer,
                                    )
                                    .await?;
                                }
                            }
                        }
                        // No routed_to = terminal branch path, nothing to enqueue
                    }

                    // Unconditional goto (supports cycles: re-enqueue even if completed)
                    if let Some(goto_id) = &node.goto {
                        let hops = goto_hops.entry(node_id.clone()).or_insert(0);
                        *hops += 1;
                        if *hops > MAX_GOTO_HOPS {
                            anyhow::bail!(
                                "goto cycle limit exceeded at node '{}' after {} hops",
                                node_id,
                                hops
                            );
                        }
                        if !in_flight.contains(goto_id.as_str()) {
                            if let Some(target_idx) = wf.nodes.iter().position(|n| &n.id == goto_id)
                            {
                                let target = wf.nodes[target_idx].clone();
                                enqueue_node_task(
                                    target,
                                    &wf,
                                    &workflow_id,
                                    &session_id,
                                    &gait_dir,
                                    &codebase,
                                    &run_worktree,
                                    &args_str,
                                    &mut state,
                                    &mut in_flight,
                                    &mut tasks,
                                    &rec,
                                    &run_log,
                                    observer,
                                )
                                .await?;
                            }
                        }
                    }

                    // Newly ready nodes (depends_on satisfied)
                    let new_ready =
                        compute_ready_nodes(&wf.nodes, &completed, &in_flight, &HashSet::new());
                    for idx in new_ready {
                        let target = wf.nodes[idx].clone();
                        enqueue_node_task(
                            target,
                            &wf,
                            &workflow_id,
                            &session_id,
                            &gait_dir,
                            &codebase,
                            &run_worktree,
                            &args_str,
                            &mut state,
                            &mut in_flight,
                            &mut tasks,
                            &rec,
                            &run_log,
                            observer,
                        )
                        .await?;
                    }
                }

                NodeTaskOutcome::GatePaused(ref out) => {
                    observer.on_event(&run_log.workflow_gate(&node_id, &out.text)?);
                    state.set_status(&node_id, NodeStatus::Paused);
                    eprintln!("[gait workflow] paused at gate '{}'", node_id);
                    return Ok(WorkflowOutcome::GatePaused);
                }

                NodeTaskOutcome::Error(ref msg) => {
                    if let Some(hooks) = &node.hooks {
                        if let Some(cmd) = &hooks.error {
                            run_hook(cmd, &env).await;
                        }
                    }
                    state.set_status(&node_id, NodeStatus::Error(msg.clone()));
                    state.failed_node = Some(node_id.clone());
                    completed.insert(node_id.clone());

                    observer.on_event(&run_log.node_error(&node_id, msg, 1)?);
                    observer.on_event(&run_log.workflow_error(&session_id, &node_id, msg)?);

                    // Proto Workflow has no workflow-level error hook; per-node hooks
                    // on the failing node are already run above via node.hooks.

                    anyhow::bail!("workflow aborted at node '{}': {}", node_id, msg);
                }

                NodeTaskOutcome::ErrorContinue => {
                    if let Some(hooks) = &node.hooks {
                        if let Some(cmd) = &hooks.error {
                            run_hook(cmd, &env).await;
                        }
                    }
                    state.set_status(&node_id, NodeStatus::Skipped);
                    completed.insert(node_id.clone());

                    // Check for newly unblocked nodes
                    let new_ready =
                        compute_ready_nodes(&wf.nodes, &completed, &in_flight, &HashSet::new());
                    for idx in new_ready {
                        let target = wf.nodes[idx].clone();
                        enqueue_node_task(
                            target,
                            &wf,
                            &workflow_id,
                            &session_id,
                            &gait_dir,
                            &codebase,
                            &run_worktree,
                            &args_str,
                            &mut state,
                            &mut in_flight,
                            &mut tasks,
                            &rec,
                            &run_log,
                            observer,
                        )
                        .await?;
                    }
                }

                NodeTaskOutcome::ErrorGoto(ref goto_id) => {
                    if let Some(hooks) = &node.hooks {
                        if let Some(cmd) = &hooks.error {
                            run_hook(cmd, &env).await;
                        }
                    }
                    state.set_status(
                        &node_id,
                        NodeStatus::Error(format!("routed to {}", goto_id)),
                    );
                    state.failed_node = Some(node_id.clone());
                    completed.insert(node_id.clone());

                    observer.on_event(&run_log.node_error(
                        &node_id,
                        &format!("on_error goto: {}", goto_id),
                        1,
                    )?);

                    // Enqueue the error-recovery node
                    if let Some(target_idx) = wf.nodes.iter().position(|n| &n.id == goto_id) {
                        if !in_flight.contains(goto_id.as_str()) {
                            let target = wf.nodes[target_idx].clone();
                            enqueue_node_task(
                                target,
                                &wf,
                                &workflow_id,
                                &session_id,
                                &gait_dir,
                                &codebase,
                                &run_worktree,
                                &args_str,
                                &mut state,
                                &mut in_flight,
                                &mut tasks,
                                &rec,
                                &run_log,
                                observer,
                            )
                            .await?;
                        }
                    }
                }
            }
        }

        emit_workflow_complete(
            &repos,
            &wf,
            &workflow_id,
            &gait_dir,
            &session_id,
            &state,
            &codebase,
            &args_str,
            &run_log,
            observer,
        )
        .await?;
        Ok(WorkflowOutcome::Completed)
    }
    .await;

    drop(rec);
    // Final drain: flush any remaining items after rec is dropped (channel closed).
    while let Ok(item) = rx.try_recv() {
        sink.item(&item);
    }
    match run_result? {
        WorkflowOutcome::GatePaused => Ok(WorkflowOutcome::GatePaused),
        WorkflowOutcome::NodeOpen => Ok(WorkflowOutcome::NodeOpen),
        WorkflowOutcome::Completed => Ok(WorkflowOutcome::Completed),
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_workflow_start(
    repos: &gait_core::git::Repos,
    wf: &Workflow,
    workflow_id: &str,
    yaml_path: &str,
    args: &[String],
    args_str: &str,
    gait_dir: &Path,
    session_id: &str,
    codebase: &Path,
    state: &WorkflowState,
    run_log: &WorkflowRunLog,
    observer: &mut dyn WorkflowObserver,
) -> anyhow::Result<()> {
    // wf is already proto Workflow — pass directly (no conversion needed).
    observer.on_event(&run_log.workflow_start(session_id, &wf.name, yaml_path, args, wf)?);

    // Note: proto Workflow does not carry workflow-level hooks (only per-node hooks via Node.hooks).
    // Workflow-level before/after hooks are a serde-schema-only feature; omitted from proto IR.
    let _ = (repos, gait_dir, workflow_id, codebase, args_str, state); // suppress unused warnings
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn emit_workflow_complete(
    repos: &gait_core::git::Repos,
    wf: &Workflow,
    workflow_id: &str,
    gait_dir: &Path,
    session_id: &str,
    state: &WorkflowState,
    codebase: &Path,
    args_str: &str,
    run_log: &WorkflowRunLog,
    observer: &mut dyn WorkflowObserver,
) -> anyhow::Result<()> {
    observer.on_event(&run_log.workflow_complete(session_id)?);

    // Note: proto Workflow does not carry workflow-level hooks.
    let _ = (repos, gait_dir, wf, workflow_id, codebase, args_str, state);
    Ok(())
}

async fn execute_node(
    node: &Node,
    ctx: &NodeContext<'_>,
    wf: &Workflow,
    input: Option<Box<dyn TurnSource>>,
) -> anyhow::Result<NodeOutput> {
    match node.body.as_ref() {
        Some(node::Body::Shell(s)) => exec_shell_node(s, ctx).await,
        Some(node::Body::Prompt(p)) => exec_prompt_node(node, p, ctx, wf, input, None).await,
        Some(node::Body::Branch(b)) => {
            let expr = substitute(&b.on, &ctx.vars);
            let mut b2 = b.clone();
            b2.on = expr;
            match resolve_branch(&b2, &ctx.vars.node_outputs) {
                Some(routed) => Ok(NodeOutput {
                    text: routed.clone(),
                    exit_code: None,
                    json: None,
                    routed_to: Some(routed),
                    session_id: String::new(),
                }),
                None => Ok(NodeOutput {
                    text: String::new(),
                    exit_code: None,
                    json: None,
                    routed_to: None,
                    session_id: String::new(),
                }),
            }
        }
        Some(node::Body::Gate(g)) => exec_gate_node(&node.id, g, &ctx.vars),
        None => anyhow::bail!("node '{}' has no body", node.id),
    }
}

fn propagate_branch_skips(
    branch_node: &Node,
    routed_to: &str,
    state: &mut WorkflowState,
    run_log: &WorkflowRunLog,
    observer: &mut dyn WorkflowObserver,
) -> anyhow::Result<()> {
    let branch_cfg = match branch_node.body.as_ref() {
        Some(node::Body::Branch(b)) => b,
        _ => return Ok(()),
    };
    let mut inactive = vec![];
    if let Some(then) = &branch_cfg.then {
        if then != routed_to {
            inactive.push(then.clone());
        }
    }
    if let Some(else_) = &branch_cfg.r#else {
        if else_ != routed_to {
            inactive.push(else_.clone());
        }
    }
    for target in branch_cfg.cases.values() {
        if target != routed_to {
            inactive.push(target.clone());
        }
    }
    for id in &inactive {
        if !state.is_done(id) && !state.is_skipped(id) {
            state.set_status(id, NodeStatus::Skipped);
            observer.on_event(
                &run_log.node_skip(id, &format!("inactive branch (routed to {})", routed_to))?,
            );
        }
    }
    Ok(())
}

fn make_env_vars(
    workflow_id: &str,
    session_id: &str,
    gait_dir: &Path,
    codebase: &Path,
    args: &str,
    node_id: Option<&str>,
    state: &WorkflowState,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("WORKFLOW_ID".into(), workflow_id.to_string());
    env.insert("SESSION_ID".into(), session_id.to_string());
    env.insert("WORKSPACE".into(), gait_dir.to_string_lossy().to_string());
    env.insert("CODEBASE".into(), codebase.to_string_lossy().to_string());
    env.insert("ARGS".into(), args.to_string());
    if let Some(id) = node_id {
        env.insert("NODE_ID".into(), id.to_string());
    }
    if let Some(f) = &state.failed_node {
        env.insert("FAILED_NODE".into(), f.clone());
    }
    env
}

#[cfg(test)]
fn topological_sort(nodes: &[Node]) -> Vec<usize> {
    use std::collections::VecDeque;
    let n = nodes.len();
    let id_to_idx: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    let mut in_degree = vec![0usize; n];
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, node) in nodes.iter().enumerate() {
        for dep in &node.depends_on {
            if let Some(&dep_idx) = id_to_idx.get(dep.as_str()) {
                adj[dep_idx].push(i);
                in_degree[i] += 1;
            }
        }
    }
    let mut queue: VecDeque<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut sorted = vec![];
    while let Some(idx) = queue.pop_front() {
        sorted.push(idx);
        for &next in &adj[idx] {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                queue.push_back(next);
            }
        }
    }
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_core::proto::gait::workflow::v1::{node, Node, PromptNode, ShellNode};

    fn node(id: &str, deps: &[&str]) -> Node {
        Node {
            id: id.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn prompt_node(id: &str, continue_session: &str) -> Node {
        Node {
            id: id.to_string(),
            body: Some(node::Body::Prompt(PromptNode {
                prompt: "do a thing".into(),
                continue_session: continue_session.to_string(),
                codebase: None,
            })),
            ..Default::default()
        }
    }

    fn shell_node(id: &str) -> Node {
        Node {
            id: id.to_string(),
            body: Some(node::Body::Shell(ShellNode {
                command: "true".into(),
                timeout_ms: None,
            })),
            ..Default::default()
        }
    }

    #[test]
    fn default_prompt_nodes_get_distinct_fresh_sessions() {
        // The core item-13 correctness claim: two ready default prompt nodes must
        // resolve to DISTINCT sessions, so their per-turn SessionLocks
        // (turn/mod.rs:154) can never collide by construction.
        let sa = node_session_id(&prompt_node("a", ""));
        let sb = node_session_id(&prompt_node("b", ""));
        assert!(
            !sa.is_empty() && !sb.is_empty(),
            "fresh sessions must be non-empty"
        );
        assert_ne!(
            sa, sb,
            "two default prompt nodes must NOT share a session id"
        );
    }

    #[test]
    fn continue_session_attaches_to_named_session() {
        assert_eq!(
            node_session_id(&prompt_node("a", "prior-sess")),
            "prior-sess"
        );
    }

    #[test]
    fn non_prompt_nodes_have_no_session() {
        assert_eq!(node_session_id(&shell_node("s")), "");
    }

    #[test]
    fn node_session_is_reused_on_re_entry_and_resume() {
        use gait_core::proto::gait::workflow::v1::{workflow_event, NodeStart, WorkflowEvent};
        // Within one run (goto/branch re-entry): second resolve reuses the first.
        let mut state = crate::state::reconstruct_state(&[]);
        let n = prompt_node("a", "");
        let first = resolve_node_session(&mut state, &n);
        let second = resolve_node_session(&mut state, &n);
        assert!(!first.is_empty());
        assert_eq!(
            first, second,
            "re-entered node must reuse its session, not re-mint"
        );

        // Across a resume: state rebuilt from the run log's NodeStart carries it back.
        let ns = WorkflowEvent {
            version: 1,
            ts: 0,
            kind: Some(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "a".into(),
                node_type: "prompt".into(),
                attempt: 1,
                session_id: first.clone(),
            })),
            ..Default::default()
        };
        let mut resumed = crate::state::reconstruct_state(&[ns]);
        assert_eq!(
            resolve_node_session(&mut resumed, &n),
            first,
            "resume must reuse the session recorded at the original NodeStart"
        );
    }

    #[test]
    fn test_compute_ready_nodes_no_deps() {
        let nodes = vec![node("a", &[]), node("b", &[]), node("c", &["a"])];
        let completed = HashSet::new();
        let in_flight: HashSet<String> = HashSet::new();
        let ready = compute_ready_nodes(&nodes, &completed, &in_flight, &HashSet::new());
        let ids: HashSet<&str> = ready.iter().map(|&i| nodes[i].id.as_str()).collect();
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));
        assert!(!ids.contains("c"));
    }

    #[test]
    fn test_compute_ready_nodes_after_completion() {
        let nodes = vec![node("a", &[]), node("b", &[]), node("c", &["a", "b"])];
        let completed: HashSet<String> = ["a".into(), "b".into()].into();
        let in_flight: HashSet<String> = HashSet::new();
        let ready = compute_ready_nodes(&nodes, &completed, &in_flight, &HashSet::new());
        let ids: Vec<&str> = ready.iter().map(|&i| nodes[i].id.as_str()).collect();
        assert!(ids.contains(&"c"));
    }

    #[test]
    fn test_compute_ready_nodes_excludes_in_flight() {
        let nodes = vec![node("a", &[]), node("b", &[])];
        let completed: HashSet<String> = HashSet::new();
        let in_flight: HashSet<String> = ["a".into()].into();
        let ready = compute_ready_nodes(&nodes, &completed, &in_flight, &HashSet::new());
        let ids: Vec<&str> = ready.iter().map(|&i| nodes[i].id.as_str()).collect();
        assert!(!ids.contains(&"a"));
        assert!(ids.contains(&"b"));
    }

    #[test]
    fn topological_sort_respects_deps() {
        let nodes = vec![node("b", &["a"]), node("a", &[])];
        let order = topological_sort(&nodes);
        let pos_a = order.iter().position(|&i| nodes[i].id == "a").unwrap();
        let pos_b = order.iter().position(|&i| nodes[i].id == "b").unwrap();
        assert!(pos_a < pos_b, "a must come before b");
    }

    #[test]
    fn ready_interactive_nodes_returns_only_dep_satisfied_interactive() {
        use gait_core::proto::gait::workflow::v1::{node, Node, PromptNode};
        fn prompt_node(id: &str, prompt: &str, deps: &[&str]) -> Node {
            Node {
                id: id.into(),
                depends_on: deps.iter().map(|s| s.to_string()).collect(),
                body: Some(node::Body::Prompt(PromptNode {
                    prompt: prompt.into(),
                    ..Default::default()
                })),
                ..Default::default()
            }
        }
        // a: interactive (empty prompt), ready. b: interactive, blocked on missing dep.
        // c: automated (has prompt), ready. d: interactive, but already completed.
        let nodes = vec![
            prompt_node("a", "", &[]),
            prompt_node("b", "", &["z"]),
            prompt_node("c", "do work", &[]),
            prompt_node("d", "", &[]),
        ];
        let completed: HashSet<String> = ["d".to_string()].into_iter().collect();
        let in_flight = HashSet::new();
        let got = ready_interactive_nodes(&nodes, &completed, &in_flight);
        // Only "a" (index 0): b blocked, c automated, d completed.
        assert_eq!(got, vec![0]);
    }

    #[test]
    fn needs_run_workspace_predicate() {
        use gait_core::proto::gait::workflow::v1::Workflow;
        let one_prompt = Workflow {
            nodes: vec![prompt_node("a", "")],
            ..Default::default()
        };
        assert!(
            !needs_run_workspace(&one_prompt),
            "lone prompt node needs no run workspace"
        );
        let one_shell = Workflow {
            nodes: vec![shell_node("s")],
            ..Default::default()
        };
        assert!(
            needs_run_workspace(&one_shell),
            "a shell node needs the shared run worktree"
        );
        let two_prompts = Workflow {
            nodes: vec![prompt_node("a", ""), prompt_node("b", "")],
            ..Default::default()
        };
        assert!(
            needs_run_workspace(&two_prompts),
            ">1 node needs the run lock"
        );
    }
}
