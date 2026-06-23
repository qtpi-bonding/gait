use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use gait_core::proto::gait::log::v1::StreamItem;
use gait_core::proto::wire;
use gait_core::Recorder;

use crate::exec_branch::resolve_branch;
use crate::exec_gate::exec_gate_node;
use crate::exec_prompt::exec_prompt_node;
use crate::exec_shell::{exec_shell_node, NodeContext};
use crate::hooks::run_hook;
use crate::progress::{init_progress, write_progress, ProgressFile, ProgressNode};
use crate::run_log::WorkflowRunLog;
use crate::schema::{NodeDef, WorkflowDef, GATE_PAUSED_SENTINEL};
use crate::state::{reconstruct_state, NodeStatus, WorkflowState};
use crate::vars::{substitute, NodeOutput, VarContext};

/// Inputs for `run_workflow`.
///
/// The provider is no longer passed in: each prompt node now resolves its own
/// `ResolvedProfile` (provider + tools + MCP) via `build_workflow_node_profile`
/// inside `exec_prompt_node`, driven through `run_loop`. This keeps workflow
/// nodes independent of any provider the CLI/REPL constructed for the parent
/// session.
pub struct WorkflowRunOpts {
    pub workflow_def: WorkflowDef,
    pub yaml_path: String,
    pub session_id: String,
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub args: Vec<String>,
}

type NodeFuture =
    Pin<Box<dyn std::future::Future<Output = anyhow::Result<(String, NodeTaskOutcome)>>>>;

#[derive(Debug)]
enum NodeTaskOutcome {
    Done(NodeOutput),
    GatePaused(NodeOutput), // carries gate message in text field
    Error(String),          // abort after retries exhausted
    ErrorContinue,          // on_error: continue — treat as skipped
    ErrorGoto(String),      // on_error: goto <node_id>
}

struct NodeRunParams {
    node: NodeDef,
    workflow_id: String,
    session_id: String,
    gait_dir: PathBuf,
    codebase: PathBuf,
    wf: Arc<WorkflowDef>,
    vars: VarContext,
    workflow_tx: tokio::sync::mpsc::UnboundedSender<StreamItem>,
}

/// Nodes whose depends_on are all satisfied, not yet completed, and not in-flight.
fn compute_ready_nodes(
    nodes: &[NodeDef],
    completed: &HashSet<String>,
    in_flight: &HashSet<String>,
) -> Vec<usize> {
    nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            !completed.contains(&n.id)
                && !in_flight.contains(&n.id)
                && n.depends_on.iter().all(|dep| completed.contains(dep))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Sync the completed set with state — any Done/Skipped/Error/Paused node is "complete"
/// for the purpose of unblocking dependents.
fn sync_completed(completed: &mut HashSet<String>, state: &WorkflowState, nodes: &[NodeDef]) {
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
/// Retry logic (retry_times, delay, backoff, context) lives here.
/// Error routing (goto, abort, continue) is determined here and signalled via outcome.
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
            gait_dir: params.gait_dir.as_path(),
            codebase: params.codebase.as_path(),
            vars: params.vars.clone(),
            workflow_tx: &params.workflow_tx,
        };

        match execute_node(&params.node, &ctx, &params.wf).await {
            Ok(out) => {
                if out.routed_to.as_deref() == Some(GATE_PAUSED_SENTINEL) {
                    return Ok((node_id, NodeTaskOutcome::GatePaused(out)));
                }
                return Ok((node_id, NodeTaskOutcome::Done(out)));
            }
            Err(e) => {
                if attempt < max_attempts {
                    continue;
                }
                // All retries exhausted — route by on_error
                if on_error.is_continue() {
                    return Ok((node_id, NodeTaskOutcome::ErrorContinue));
                }
                if let Some(goto_id) = on_error.goto_node() {
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
    node: NodeDef,
    wf: &Arc<WorkflowDef>,
    workflow_id: &str,
    session_id: &str,
    gait_dir: &Path,
    codebase: &Path,
    args_str: &str,
    state: &mut WorkflowState,
    in_flight: &mut HashSet<String>,
    tasks: &mut FuturesUnordered<NodeFuture>,
    rec: &Recorder,
    run_log: &WorkflowRunLog,
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
            rec.record(wire::raw(
                "hook_run",
                serde_json::json!({
                    "workflow_id": workflow_id, "node_id": node_id.clone(),
                    "hook_type": "before", "cmd": cmd.clone(), "exit_code": exit,
                }),
            ))?;
        }
    }

    rec.record(wire::raw(
        "node_start",
        serde_json::json!({
            "workflow_id": workflow_id, "node_id": node_id.clone(),
            "node_type": node.node_type_name(), "attempt": 1,
        }),
    ))?;
    run_log.node_start(&node_id, node.node_type_name(), 1, session_id)?;
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
        gait_dir: gait_dir.to_path_buf(),
        codebase: codebase.to_path_buf(),
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
}

pub async fn run_workflow(opts: WorkflowRunOpts) -> anyhow::Result<WorkflowOutcome> {
    let WorkflowRunOpts {
        workflow_def: wf_owned,
        yaml_path,
        session_id,
        gait_dir,
        codebase,
        args,
    } = opts;
    let wf = Arc::new(wf_owned);

    let repos = gait_core::git::Repos::production();
    repos
        .traces
        .create_session_worktree(&gait_dir, &session_id)?;
    let session_dir = gait_dir.join("sessions").join(&session_id);
    let tx_path = session_dir.join("log.jsonl");

    let existing_events = gait_core::session::log::read_events(&tx_path).unwrap_or_default();
    let mut state = reconstruct_state(&existing_events);

    let workflow_id = if state.workflow_id.is_empty() {
        session_id.clone()
    } else {
        state.workflow_id.clone()
    };

    let args_str = args.join(" ");
    // The per-run pbjson event log lives alongside the session transcript. It is
    // created once at run start and opened (not recreated) on resume after a gate.
    let workflow_runs_dir = gait_dir.join("workflow-runs");
    let run_log = if tx_path.exists() {
        // Resumed run: re-open the existing log (don't truncate)
        WorkflowRunLog::open(&workflow_runs_dir, &session_id)?
    } else {
        WorkflowRunLog::create(&workflow_runs_dir, &session_id)?
    };

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();
    let rec = Recorder::for_session(&gait_dir, &session_id, tx);
    let consumer = tokio::spawn(async move {
        let mut sink = gait_core::embed::CliSink::stdio(false);
        gait_core::port::output::drive(rx, &mut sink).await;
        sink.failed
    });

    let run_result: anyhow::Result<WorkflowOutcome> =
        async {
            if existing_events.is_empty() {
                emit_workflow_start(
                    &rec,
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

            // Enqueue all initially ready nodes
            let initially_ready = compute_ready_nodes(&wf.nodes, &completed, &in_flight);
            for idx in initially_ready {
                let node = wf.nodes[idx].clone();
                enqueue_node_task(
                    node,
                    &wf,
                    &workflow_id,
                    &session_id,
                    &gait_dir,
                    &codebase,
                    &args_str,
                    &mut state,
                    &mut in_flight,
                    &mut tasks,
                    &rec,
                    &run_log,
                )
                .await?;
            }

            loop {
                if tasks.is_empty() {
                    break;
                }

                let (node_id, outcome) = match tasks.next().await {
                    Some(Ok(r)) => r,
                    Some(Err(e)) => return Err(e),
                    None => break,
                };
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
                    NodeTaskOutcome::Done(ref out) => {
                        rec.record(wire::raw(
                            "node_complete",
                            serde_json::json!({
                                "workflow_id": workflow_id.clone(), "node_id": node_id.clone(),
                                "output": out.text.clone(), "exit_code": out.exit_code,
                                "json_output": out.json.clone(), "routed_to": out.routed_to.clone(),
                            }),
                        ))?;
                        run_log.node_complete(
                            &node_id,
                            &session_id,
                            &out.text,
                            out.exit_code,
                            &out.json.as_ref().map(|v| v.to_string()).unwrap_or_default(),
                            out.routed_to.as_deref().unwrap_or(""),
                        )?;
                        state.set_status(&node_id, NodeStatus::Done);
                        state.node_outputs.insert(node_id.clone(), out.clone());
                        completed.insert(node_id.clone());

                        // After hooks
                        if let Some(hooks) = &node.hooks {
                            if let Some(cmd) = &hooks.after {
                                let (exit, _) = run_hook(cmd, &env).await;
                                rec.record(wire::raw("hook_run", serde_json::json!({
                                "workflow_id": workflow_id.clone(), "node_id": node_id.clone(),
                                "hook_type": "after", "cmd": cmd.clone(), "exit_code": exit,
                            })))?;
                            }
                        }

                        // Branch routing: mark non-taken paths as skipped, enqueue taken path
                        if node.branch.is_some() {
                            if let Some(routed_to) = &out.routed_to {
                                propagate_branch_skips(
                                    &node,
                                    routed_to,
                                    &mut state,
                                    &rec,
                                    &workflow_id,
                                    &run_log,
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
                                            &args_str,
                                            &mut state,
                                            &mut in_flight,
                                            &mut tasks,
                                            &rec,
                                            &run_log,
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
                                if let Some(target_idx) =
                                    wf.nodes.iter().position(|n| &n.id == goto_id)
                                {
                                    let target = wf.nodes[target_idx].clone();
                                    enqueue_node_task(
                                        target,
                                        &wf,
                                        &workflow_id,
                                        &session_id,
                                        &gait_dir,
                                        &codebase,
                                        &args_str,
                                        &mut state,
                                        &mut in_flight,
                                        &mut tasks,
                                        &rec,
                                        &run_log,
                                    )
                                    .await?;
                                }
                            }
                        }

                        // Newly ready nodes (depends_on satisfied)
                        let new_ready = compute_ready_nodes(&wf.nodes, &completed, &in_flight);
                        for idx in new_ready {
                            let target = wf.nodes[idx].clone();
                            enqueue_node_task(
                                target,
                                &wf,
                                &workflow_id,
                                &session_id,
                                &gait_dir,
                                &codebase,
                                &args_str,
                                &mut state,
                                &mut in_flight,
                                &mut tasks,
                                &rec,
                                &run_log,
                            )
                            .await?;
                        }
                    }

                    NodeTaskOutcome::GatePaused(ref out) => {
                        rec.record(wire::raw(
                            "workflow_gate",
                            serde_json::json!({
                                "workflow_id": workflow_id.clone(),
                                "node_id": node_id.clone(),
                                "message": out.text.clone(),
                            }),
                        ))?;
                        run_log.workflow_gate(&node_id, &out.text)?;
                        state.set_status(&node_id, NodeStatus::Paused);
                        update_progress(
                            &gait_dir,
                            &session_id,
                            &workflow_id,
                            &wf.name,
                            &state,
                            &wf.nodes,
                        )?;
                        repos.traces.commit_session(
                            &gait_dir,
                            &session_id,
                            &format!("workflow: gate {}", node_id),
                        )?;
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

                        rec.record(wire::raw(
                            "node_error",
                            serde_json::json!({
                                "workflow_id": workflow_id.clone(), "node_id": node_id.clone(),
                                "error": msg.clone(), "attempt": 1,
                            }),
                        ))?;
                        rec.record(wire::raw(
                            "workflow_error",
                            serde_json::json!({
                                "workflow_id": workflow_id.clone(), "failed_node": node_id.clone(),
                                "error": msg.clone(),
                            }),
                        ))?;
                        run_log.node_error(&node_id, msg, 1)?;
                        run_log.workflow_error(&session_id, &node_id, msg)?;

                        if let Some(hooks) = &wf.hooks {
                            if let Some(cmd) = &hooks.error {
                                let mut env2 = make_env_vars(
                                    &workflow_id,
                                    &session_id,
                                    &gait_dir,
                                    &codebase,
                                    &args_str,
                                    None,
                                    &state,
                                );
                                env2.insert("FAILED_NODE".into(), node_id.clone());
                                run_hook(cmd, &env2).await;
                            }
                        }

                        update_progress(
                            &gait_dir,
                            &session_id,
                            &workflow_id,
                            &wf.name,
                            &state,
                            &wf.nodes,
                        )?;
                        repos.traces.commit_session(
                            &gait_dir,
                            &session_id,
                            &format!("workflow: error at {}", node_id),
                        )?;
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
                        let new_ready = compute_ready_nodes(&wf.nodes, &completed, &in_flight);
                        for idx in new_ready {
                            let target = wf.nodes[idx].clone();
                            enqueue_node_task(
                                target,
                                &wf,
                                &workflow_id,
                                &session_id,
                                &gait_dir,
                                &codebase,
                                &args_str,
                                &mut state,
                                &mut in_flight,
                                &mut tasks,
                                &rec,
                                &run_log,
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

                        rec.record(wire::raw(
                            "node_error",
                            serde_json::json!({
                                "workflow_id": workflow_id.clone(), "node_id": node_id.clone(),
                                "error": format!("on_error goto: {}", goto_id), "attempt": 1,
                            }),
                        ))?;
                        run_log.node_error(&node_id, &format!("on_error goto: {}", goto_id), 1)?;

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
                                    &args_str,
                                    &mut state,
                                    &mut in_flight,
                                    &mut tasks,
                                    &rec,
                                    &run_log,
                                )
                                .await?;
                            }
                        }
                    }
                }

                update_progress(
                    &gait_dir,
                    &session_id,
                    &workflow_id,
                    &wf.name,
                    &state,
                    &wf.nodes,
                )?;
                repos.traces.commit_session(
                    &gait_dir,
                    &session_id,
                    &format!("workflow: {}", node_id),
                )?;
            }

            emit_workflow_complete(
                &rec,
                &repos,
                &wf,
                &workflow_id,
                &gait_dir,
                &session_id,
                &state,
                &codebase,
                &args_str,
                &run_log,
            )
            .await?;
            Ok(WorkflowOutcome::Completed)
        }
        .await;

    drop(rec);
    let _failed = consumer.await.unwrap_or(false);
    match run_result? {
        WorkflowOutcome::GatePaused => Ok(WorkflowOutcome::GatePaused),
        WorkflowOutcome::Completed => Ok(WorkflowOutcome::Completed),
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_workflow_start(
    rec: &Recorder,
    repos: &gait_core::git::Repos,
    wf: &WorkflowDef,
    workflow_id: &str,
    yaml_path: &str,
    args: &[String],
    args_str: &str,
    gait_dir: &Path,
    session_id: &str,
    codebase: &Path,
    state: &WorkflowState,
    run_log: &WorkflowRunLog,
) -> anyhow::Result<()> {
    rec.record(wire::raw(
        "workflow_start",
        serde_json::json!({
            "workflow_id": workflow_id,
            "name": wf.name,
            "yaml_path": yaml_path,
            "args": args,
        }),
    ))?;
    run_log.workflow_start(session_id, &wf.name, yaml_path, args)?;
    let node_ids: Vec<&str> = wf.nodes.iter().map(|n| n.id.as_str()).collect();
    let progress = init_progress(workflow_id, &wf.name, &node_ids);
    write_progress(gait_dir, session_id, &progress)?;
    repos
        .traces
        .commit_session(gait_dir, session_id, "workflow: start")?;

    if let Some(hooks) = &wf.hooks {
        if let Some(cmd) = &hooks.before {
            let env = make_env_vars(
                workflow_id,
                session_id,
                gait_dir,
                codebase,
                args_str,
                None,
                state,
            );
            let (exit, _) = run_hook(cmd, &env).await;
            rec.record(wire::raw(
                "hook_run",
                serde_json::json!({
                    "workflow_id": workflow_id, "node_id": null,
                    "hook_type": "before", "cmd": cmd, "exit_code": exit,
                }),
            ))?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn emit_workflow_complete(
    rec: &Recorder,
    repos: &gait_core::git::Repos,
    wf: &WorkflowDef,
    workflow_id: &str,
    gait_dir: &Path,
    session_id: &str,
    state: &WorkflowState,
    codebase: &Path,
    args_str: &str,
    run_log: &WorkflowRunLog,
) -> anyhow::Result<()> {
    rec.record(wire::raw(
        "workflow_complete",
        serde_json::json!({
            "workflow_id": workflow_id,
        }),
    ))?;
    run_log.workflow_complete(session_id)?;
    update_progress_done(
        gait_dir,
        session_id,
        workflow_id,
        &wf.name,
        state,
        &wf.nodes,
    )?;
    repos
        .traces
        .commit_session(gait_dir, session_id, "workflow: complete")?;

    if let Some(hooks) = &wf.hooks {
        if let Some(cmd) = &hooks.after {
            let env = make_env_vars(
                workflow_id,
                session_id,
                gait_dir,
                codebase,
                args_str,
                None,
                state,
            );
            run_hook(cmd, &env).await;
        }
    }

    eprintln!("[gait workflow] complete: {}", wf.name);
    Ok(())
}

async fn execute_node(
    node: &NodeDef,
    ctx: &NodeContext<'_>,
    wf: &WorkflowDef,
) -> anyhow::Result<NodeOutput> {
    if let Some(shell) = &node.shell {
        return exec_shell_node(shell, ctx).await;
    }
    if let Some(prompt) = &node.prompt {
        return exec_prompt_node(node, prompt, ctx, wf).await;
    }
    if let Some(branch_cfg) = &node.branch {
        let expr = substitute(&branch_cfg.on, &ctx.vars);
        let mut cfg = branch_cfg.clone();
        cfg.on = expr;
        return match resolve_branch(&cfg, &ctx.vars.node_outputs) {
            Some(routed) => Ok(NodeOutput {
                text: routed.clone(),
                exit_code: None,
                json: None,
                routed_to: Some(routed),
            }),
            // No route = terminal branch path (no else, condition was false)
            None => Ok(NodeOutput {
                text: String::new(),
                exit_code: None,
                json: None,
                routed_to: None,
            }),
        };
    }
    if let Some(gate_cfg) = &node.gate {
        return exec_gate_node(&node.id, gate_cfg, &ctx.vars);
    }
    anyhow::bail!("node '{}' has no valid type", node.id)
}

fn propagate_branch_skips(
    branch_node: &NodeDef,
    routed_to: &str,
    state: &mut WorkflowState,
    rec: &Recorder,
    workflow_id: &str,
    run_log: &WorkflowRunLog,
) -> anyhow::Result<()> {
    let branch_cfg = branch_node.branch.as_ref().unwrap();
    let mut inactive = vec![];
    if let Some(then) = &branch_cfg.then {
        if then != routed_to {
            inactive.push(then.clone());
        }
    }
    if let Some(else_) = &branch_cfg.else_ {
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
            rec.record(wire::raw(
                "node_skip",
                serde_json::json!({
                    "workflow_id": workflow_id.to_string(), "node_id": id.clone(),
                    "reason": format!("inactive branch (routed to {})", routed_to),
                }),
            ))?;
            run_log.node_skip(id, &format!("inactive branch (routed to {})", routed_to))?;
        }
    }
    Ok(())
}

fn update_progress(
    gait_dir: &Path,
    session_id: &str,
    workflow_id: &str,
    name: &str,
    state: &WorkflowState,
    nodes: &[NodeDef],
) -> anyhow::Result<()> {
    let pf = build_progress(workflow_id, name, "running", state, nodes);
    write_progress(gait_dir, session_id, &pf)
}

fn update_progress_done(
    gait_dir: &Path,
    session_id: &str,
    workflow_id: &str,
    name: &str,
    state: &WorkflowState,
    nodes: &[NodeDef],
) -> anyhow::Result<()> {
    let pf = build_progress(workflow_id, name, "done", state, nodes);
    write_progress(gait_dir, session_id, &pf)
}

fn build_progress(
    workflow_id: &str,
    name: &str,
    wf_status: &str,
    state: &WorkflowState,
    nodes: &[NodeDef],
) -> ProgressFile {
    ProgressFile {
        workflow_id: workflow_id.to_string(),
        name: name.to_string(),
        status: wf_status.to_string(),
        nodes: nodes
            .iter()
            .map(|n| {
                let st = state.node_status(&n.id);
                let out = state.node_outputs.get(&n.id);
                ProgressNode {
                    id: n.id.clone(),
                    status: match &st {
                        NodeStatus::Pending => "pending",
                        NodeStatus::Running => "running",
                        NodeStatus::Done => "done",
                        NodeStatus::Skipped => "skipped",
                        NodeStatus::Error(_) => "error",
                        NodeStatus::Paused => "paused",
                    }
                    .to_string(),
                    exit: out.and_then(|o| o.exit_code),
                    iter: None,
                    routed_to: out.and_then(|o| o.routed_to.clone()),
                    paused_at: if st == NodeStatus::Paused {
                        Some(gait_core::session::log::now_ms().to_string())
                    } else {
                        None
                    },
                    error: if let NodeStatus::Error(e) = st {
                        Some(e)
                    } else {
                        None
                    },
                }
            })
            .collect(),
    }
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
fn topological_sort(nodes: &[NodeDef]) -> Vec<usize> {
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

    fn node(id: &str, deps: &[&str]) -> NodeDef {
        NodeDef {
            id: id.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn test_compute_ready_nodes_no_deps() {
        let nodes = vec![node("a", &[]), node("b", &[]), node("c", &["a"])];
        let completed = HashSet::new();
        let in_flight: HashSet<String> = HashSet::new();
        let ready = compute_ready_nodes(&nodes, &completed, &in_flight);
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
        let ready = compute_ready_nodes(&nodes, &completed, &in_flight);
        let ids: Vec<&str> = ready.iter().map(|&i| nodes[i].id.as_str()).collect();
        assert!(ids.contains(&"c"));
    }

    #[test]
    fn test_compute_ready_nodes_excludes_in_flight() {
        let nodes = vec![node("a", &[]), node("b", &[])];
        let completed: HashSet<String> = HashSet::new();
        let in_flight: HashSet<String> = ["a".into()].into();
        let ready = compute_ready_nodes(&nodes, &completed, &in_flight);
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
}
