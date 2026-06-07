use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::provider::Provider;
use crate::workflow::schema::{WorkflowDef, NodeDef};
use crate::workflow::vars::{NodeOutput, VarContext, substitute};
use crate::workflow::progress::{ProgressFile, ProgressNode, write_progress, init_progress};
use crate::workflow::state::{WorkflowState, NodeStatus, reconstruct_state};
use crate::workflow::hooks::run_hook;
use crate::workflow::exec_shell::{NodeContext, exec_shell_node};
use crate::workflow::exec_prompt::exec_prompt_node;
use crate::workflow::exec_loop::exec_loop_node;
use crate::workflow::exec_branch::resolve_branch;
use crate::workflow::exec_gate::exec_gate_node;
use crate::session::transcript::{AnyEvent, emit_event, read_events, now_ms};
use crate::git;

pub struct WorkflowRunOpts {
    pub workflow_def: WorkflowDef,
    pub yaml_path: String,
    pub session_id: String,
    pub workspace: PathBuf,
    pub codebase: PathBuf,
    pub provider: Arc<dyn Provider>,
    pub args: Vec<String>,
}

pub async fn run_workflow(opts: WorkflowRunOpts) -> anyhow::Result<()> {
    let WorkflowRunOpts { workflow_def: wf, yaml_path, session_id, workspace, codebase, provider, args } = opts;

    git::create_session_worktree(&workspace, &session_id)?;
    let session_dir = workspace.join("sessions").join(&session_id);
    let tx_path = session_dir.join("transcript.jsonl");

    let existing_events = read_events(&tx_path).unwrap_or_default();
    let mut state = reconstruct_state(&existing_events);

    let workflow_id = if state.workflow_id.is_empty() {
        session_id.clone()
    } else {
        state.workflow_id.clone()
    };

    let args_str = args.join(" ");

    if existing_events.is_empty() {
        emit_event(&tx_path, &AnyEvent::WorkflowStart {
            workflow_id: workflow_id.clone(),
            name: wf.name.clone(),
            yaml_path: yaml_path.clone(),
            args: args.clone(),
            ts: now_ms(),
        })?;
        let node_ids: Vec<&str> = wf.nodes.iter().map(|n| n.id.as_str()).collect();
        let progress = init_progress(&workflow_id, &wf.name, &node_ids);
        write_progress(&workspace, &session_id, &progress)?;
        git::commit_session(&workspace, &session_id, "workflow: start")?;

        if let Some(hooks) = &wf.hooks {
            if let Some(cmd) = &hooks.before {
                let env = make_env_vars(&workflow_id, &session_id, &workspace, &codebase, &args_str, None, &state);
                let (exit, _) = run_hook(cmd, &env).await;
                emit_event(&tx_path, &AnyEvent::HookRun {
                    workflow_id: workflow_id.clone(), node_id: None,
                    hook_type: "before".into(), cmd: cmd.clone(), exit_code: exit, ts: now_ms(),
                })?;
            }
        }
    }

    if let Some(gate_id) = state.gate_paused.take() {
        state.set_status(&gate_id, NodeStatus::Done);
    }

    let order = topological_sort(&wf.nodes);

    for idx in order {
        let node = &wf.nodes[idx];
        let node_id = node.id.clone();

        match state.node_status(&node_id) {
            NodeStatus::Done | NodeStatus::Skipped => continue,
            NodeStatus::Error(_) => continue,
            _ => {}
        }

        if should_skip_node(node, &state) {
            emit_event(&tx_path, &AnyEvent::NodeSkip {
                workflow_id: workflow_id.clone(), node_id: node_id.clone(),
                reason: "inactive branch path".into(), ts: now_ms(),
            })?;
            state.set_status(&node_id, NodeStatus::Skipped);
            update_progress(&workspace, &session_id, &workflow_id, &wf.name, &state, &wf.nodes)?;
            git::commit_session(&workspace, &session_id, &format!("workflow: skip {}", node_id))?;
            continue;
        }

        let env = make_env_vars(&workflow_id, &session_id, &workspace, &codebase, &args_str, Some(&node_id), &state);
        if let Some(hooks) = &node.hooks {
            if let Some(cmd) = &hooks.before {
                let (exit, _) = run_hook(cmd, &env).await;
                emit_event(&tx_path, &AnyEvent::HookRun {
                    workflow_id: workflow_id.clone(), node_id: Some(node_id.clone()),
                    hook_type: "before".into(), cmd: cmd.clone(), exit_code: exit, ts: now_ms(),
                })?;
            }
        }

        emit_event(&tx_path, &AnyEvent::NodeStart {
            workflow_id: workflow_id.clone(), node_id: node_id.clone(),
            node_type: node.node_type_name().into(), attempt: 1, ts: now_ms(),
        })?;
        state.set_status(&node_id, NodeStatus::Running);

        let vars = VarContext {
            workflow_id: workflow_id.clone(), session_id: session_id.clone(),
            workspace: workspace.clone(), codebase: codebase.clone(),
            args: args_str.clone(), node_id: node_id.clone(),
            node_outputs: state.node_outputs.clone(),
            loop_iter: None, loop_max: None, loop_output: None,
            loop_signal: None, node_exit: None, failed_node: state.failed_node.clone(),
        };
        let ctx = NodeContext { workflow_id: &workflow_id, session_id: &session_id, workspace: &workspace, codebase: &codebase, vars };

        let retry_max = node.retry.as_ref().map(|r| r.max).unwrap_or(1);
        let retry_delay = node.retry.as_ref().map(|r| r.delay).unwrap_or(0);
        let mut node_result: Option<NodeOutput> = None;
        let mut last_error = String::new();

        for attempt in 1..=(retry_max as usize) {
            if attempt > 1 {
                tokio::time::sleep(std::time::Duration::from_millis(retry_delay)).await;
            }
            let result = execute_node(node, &ctx, &wf, provider.clone(), &tx_path, &workflow_id, &session_id, &workspace).await;
            match result {
                Ok(out) => { node_result = Some(out); break; }
                Err(e) => {
                    last_error = e.to_string();
                    emit_event(&tx_path, &AnyEvent::NodeError {
                        workflow_id: workflow_id.clone(), node_id: node_id.clone(),
                        error: last_error.clone(), attempt: attempt as u32, ts: now_ms(),
                    })?;
                }
            }
        }

        match node_result {
            Some(out) => {
                if out.routed_to.as_deref() == Some("__gate_paused__") {
                    state.set_status(&node_id, NodeStatus::Paused);
                    update_progress(&workspace, &session_id, &workflow_id, &wf.name, &state, &wf.nodes)?;
                    git::commit_session(&workspace, &session_id, &format!("workflow: gate {}", node_id))?;
                    eprintln!("[gait workflow] paused at gate '{}'", node_id);
                    std::process::exit(0);
                }

                if node.branch.is_some() {
                    let routed = out.routed_to.clone().unwrap_or_default();
                    propagate_branch_skips(node, &routed, &wf.nodes, &mut state, &tx_path, &workflow_id)?;
                }

                emit_event(&tx_path, &AnyEvent::NodeComplete {
                    workflow_id: workflow_id.clone(), node_id: node_id.clone(),
                    output: out.text.clone(), exit_code: out.exit_code,
                    json_output: out.json.clone(), routed_to: out.routed_to.clone(), ts: now_ms(),
                })?;
                state.set_status(&node_id, NodeStatus::Done);
                state.node_outputs.insert(node_id.clone(), out);

                if let Some(hooks) = &node.hooks {
                    if let Some(cmd) = &hooks.after {
                        let env = make_env_vars(&workflow_id, &session_id, &workspace, &codebase, &args_str, Some(&node_id), &state);
                        let (exit, _) = run_hook(cmd, &env).await;
                        emit_event(&tx_path, &AnyEvent::HookRun {
                            workflow_id: workflow_id.clone(), node_id: Some(node_id.clone()),
                            hook_type: "after".into(), cmd: cmd.clone(), exit_code: exit, ts: now_ms(),
                        })?;
                    }
                }
            }
            None => {
                if let Some(hooks) = &node.hooks {
                    if let Some(cmd) = &hooks.error {
                        let env = make_env_vars(&workflow_id, &session_id, &workspace, &codebase, &args_str, Some(&node_id), &state);
                        run_hook(cmd, &env).await;
                    }
                }
                state.set_status(&node_id, NodeStatus::Error(last_error.clone()));

                let on_error = &node.on_error;
                if on_error.is_abort() {
                    emit_event(&tx_path, &AnyEvent::WorkflowError {
                        workflow_id: workflow_id.clone(), failed_node: node_id.clone(),
                        error: last_error.clone(), ts: now_ms(),
                    })?;
                    update_progress(&workspace, &session_id, &workflow_id, &wf.name, &state, &wf.nodes)?;
                    git::commit_session(&workspace, &session_id, &format!("workflow: error at {}", node_id))?;
                    if let Some(hooks) = &wf.hooks {
                        if let Some(cmd) = &hooks.error {
                            let mut env = make_env_vars(&workflow_id, &session_id, &workspace, &codebase, &args_str, None, &state);
                            env.insert("FAILED_NODE".into(), node_id.clone());
                            run_hook(cmd, &env).await;
                        }
                    }
                    anyhow::bail!("workflow aborted at node '{}': {}", node_id, last_error);
                } else if on_error.is_continue() {
                    state.set_status(&node_id, NodeStatus::Skipped);
                } else if let Some(target) = on_error.goto_node() {
                    eprintln!("[gait workflow] on_error goto: {}", target);
                    // target node remains pending, will be picked up in next iteration
                    let _ = target;
                }
            }
        }

        update_progress(&workspace, &session_id, &workflow_id, &wf.name, &state, &wf.nodes)?;
        git::commit_session(&workspace, &session_id, &format!("workflow: {}", node_id))?;
    }

    emit_event(&tx_path, &AnyEvent::WorkflowComplete { workflow_id: workflow_id.clone(), ts: now_ms() })?;
    update_progress_done(&workspace, &session_id, &workflow_id, &wf.name, &state, &wf.nodes)?;
    git::commit_session(&workspace, &session_id, "workflow: complete")?;

    if let Some(hooks) = &wf.hooks {
        if let Some(cmd) = &hooks.after {
            let env = make_env_vars(&workflow_id, &session_id, &workspace, &codebase, &args_str, None, &state);
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
    provider: Arc<dyn Provider>,
    tx_path: &Path,
    workflow_id: &str,
    session_id: &str,
    workspace: &Path,
) -> anyhow::Result<NodeOutput> {
    if let Some(shell) = &node.shell {
        return exec_shell_node(shell, ctx).await;
    }
    if let Some(prompt) = &node.prompt {
        return exec_prompt_node(node, prompt, ctx, wf, provider).await;
    }
    if let Some(loop_cfg) = &node.loop_node {
        return exec_loop_node(node, loop_cfg, ctx, wf, provider).await;
    }
    if let Some(branch_cfg) = &node.branch {
        let expr = substitute(&branch_cfg.on, &ctx.vars);
        let mut cfg = branch_cfg.clone();
        cfg.on = expr;
        let routed = resolve_branch(&cfg, &ctx.vars.node_outputs)
            .ok_or_else(|| anyhow::anyhow!("branch '{}' has no route", node.id))?;
        return Ok(NodeOutput { text: routed.clone(), exit_code: None, json: None, routed_to: Some(routed) });
    }
    if let Some(gate_cfg) = &node.gate {
        return exec_gate_node(workflow_id, &node.id, gate_cfg, &ctx.vars, tx_path);
    }
    anyhow::bail!("node '{}' has no valid type", node.id)
}

fn should_skip_node(node: &NodeDef, state: &WorkflowState) -> bool {
    if node.depends_on.is_empty() { return false; }
    let all_resolved = node.depends_on.iter().all(|dep| {
        matches!(state.node_status(dep), NodeStatus::Done | NodeStatus::Skipped | NodeStatus::Error(_))
    });
    if !all_resolved { return false; }
    let any_done = node.depends_on.iter().any(|dep| state.is_done(dep));
    let all_skipped = node.depends_on.iter().all(|dep| state.is_skipped(dep));
    all_skipped && !any_done
}

fn propagate_branch_skips(
    branch_node: &NodeDef,
    routed_to: &str,
    all_nodes: &[NodeDef],
    state: &mut WorkflowState,
    tx_path: &Path,
    workflow_id: &str,
) -> anyhow::Result<()> {
    let branch_cfg = branch_node.branch.as_ref().unwrap();
    let mut inactive = vec![];
    if let Some(then) = &branch_cfg.then {
        if then != routed_to { inactive.push(then.clone()); }
    }
    if let Some(else_) = &branch_cfg.else_ {
        if else_ != routed_to { inactive.push(else_.clone()); }
    }
    for (_key, target) in &branch_cfg.cases {
        if target != routed_to { inactive.push(target.clone()); }
    }
    for id in &inactive {
        if !state.is_done(id) && !state.is_skipped(id) {
            state.set_status(id, NodeStatus::Skipped);
            emit_event(tx_path, &AnyEvent::NodeSkip {
                workflow_id: workflow_id.to_string(), node_id: id.clone(),
                reason: format!("inactive branch (routed to {})", routed_to), ts: now_ms(),
            })?;
        }
    }
    let _ = all_nodes;
    Ok(())
}

fn update_progress(workspace: &Path, session_id: &str, workflow_id: &str, name: &str, state: &WorkflowState, nodes: &[NodeDef]) -> anyhow::Result<()> {
    let pf = build_progress(workflow_id, name, "running", state, nodes);
    write_progress(workspace, session_id, &pf)
}

fn update_progress_done(workspace: &Path, session_id: &str, workflow_id: &str, name: &str, state: &WorkflowState, nodes: &[NodeDef]) -> anyhow::Result<()> {
    let pf = build_progress(workflow_id, name, "done", state, nodes);
    write_progress(workspace, session_id, &pf)
}

fn build_progress(workflow_id: &str, name: &str, wf_status: &str, state: &WorkflowState, nodes: &[NodeDef]) -> ProgressFile {
    ProgressFile {
        workflow_id: workflow_id.to_string(),
        name: name.to_string(),
        status: wf_status.to_string(),
        nodes: nodes.iter().map(|n| {
            let st = state.node_status(&n.id);
            let out = state.node_outputs.get(&n.id);
            ProgressNode {
                id: n.id.clone(),
                status: match &st {
                    NodeStatus::Pending => "pending", NodeStatus::Running => "running",
                    NodeStatus::Done => "done", NodeStatus::Skipped => "skipped",
                    NodeStatus::Error(_) => "error", NodeStatus::Paused => "paused",
                }.to_string(),
                exit: out.and_then(|o| o.exit_code),
                iter: None,
                routed_to: out.and_then(|o| o.routed_to.clone()),
                paused_at: if st == NodeStatus::Paused { Some(now_ms().to_string()) } else { None },
                error: if let NodeStatus::Error(e) = st { Some(e) } else { None },
            }
        }).collect(),
    }
}

fn make_env_vars(workflow_id: &str, session_id: &str, workspace: &Path, codebase: &Path, args: &str, node_id: Option<&str>, state: &WorkflowState) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("WORKFLOW_ID".into(), workflow_id.to_string());
    env.insert("SESSION_ID".into(), session_id.to_string());
    env.insert("WORKSPACE".into(), workspace.to_string_lossy().to_string());
    env.insert("CODEBASE".into(), codebase.to_string_lossy().to_string());
    env.insert("ARGS".into(), args.to_string());
    if let Some(id) = node_id { env.insert("NODE_ID".into(), id.to_string()); }
    if let Some(f) = &state.failed_node { env.insert("FAILED_NODE".into(), f.clone()); }
    env
}

pub fn topological_sort(nodes: &[NodeDef]) -> Vec<usize> {
    use std::collections::VecDeque;
    let n = nodes.len();
    let id_to_idx: HashMap<&str, usize> = nodes.iter().enumerate()
        .map(|(i, node)| (node.id.as_str(), i)).collect();
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
            if in_degree[next] == 0 { queue.push_back(next); }
        }
    }
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topological_sort_respects_deps() {
        let nodes = vec![
            crate::workflow::schema::NodeDef {
                id: "b".into(), depends_on: vec!["a".into()],
                on_error: Default::default(), retry: None, hooks: None,
                model: None, auto: None, allow: None, agent: None, system_prompt: None,
                codebase: None, files: None, output_schema: None,
                shell: Some(crate::workflow::schema::ShellConfig::Short("echo b".into())),
                prompt: None, loop_node: None, branch: None, gate: None,
            },
            crate::workflow::schema::NodeDef {
                id: "a".into(), depends_on: vec![],
                on_error: Default::default(), retry: None, hooks: None,
                model: None, auto: None, allow: None, agent: None, system_prompt: None,
                codebase: None, files: None, output_schema: None,
                shell: Some(crate::workflow::schema::ShellConfig::Short("echo a".into())),
                prompt: None, loop_node: None, branch: None, gate: None,
            },
        ];
        let order = topological_sort(&nodes);
        let pos_a = order.iter().position(|&i| nodes[i].id == "a").unwrap();
        let pos_b = order.iter().position(|&i| nodes[i].id == "b").unwrap();
        assert!(pos_a < pos_b, "a must come before b");
    }
}
