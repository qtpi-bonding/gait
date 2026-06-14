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
use crate::session::transcript::{read_events, now_ms};
use crate::proto::wire;

pub struct WorkflowRunOpts {
    pub workflow_def: WorkflowDef,
    pub yaml_path: String,
    pub session_id: String,
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub provider: Arc<dyn Provider>,
    pub args: Vec<String>,
}

pub async fn run_workflow(opts: WorkflowRunOpts) -> anyhow::Result<()> {
    let WorkflowRunOpts { workflow_def: wf, yaml_path, session_id, gait_dir, codebase, provider, args } = opts;

    let repos = crate::git::Repos::production();
    repos.traces.create_session_worktree(&gait_dir, &session_id)?;
    let session_dir = gait_dir.join("sessions").join(&session_id);
    let tx_path = session_dir.join("log.jsonl");

    let existing_events = read_events(&tx_path).unwrap_or_default();
    let mut state = reconstruct_state(&existing_events);

    let workflow_id = if state.workflow_id.is_empty() {
        session_id.clone()
    } else {
        state.workflow_id.clone()
    };

    let args_str = args.join(" ");

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let rec = crate::proto::Recorder::for_session(&gait_dir, &session_id, tx);
    // Consume the channel concurrently with the node loop: Trace→stderr, Error→stderr.
    let consumer = tokio::spawn(async move {
        let mut sink = crate::cli::output::StdioSink::stdio(false);
        crate::port::output::drive(rx, &mut sink).await;
        sink.failed
    });

    // The node loop runs inside this block so that, on EVERY exit path (success,
    // bail!, or gate-pause), we fall through to drop(rec) + consumer.await below
    // and flush the channel before the process exits. The gate-pause path must
    // NOT std::process::exit in place — it returns GatePaused and the outer scope
    // exits 0 only after draining.
    enum Outcome { Completed, GatePaused }

    let run_result: anyhow::Result<Outcome> = async {
    if existing_events.is_empty() {
        rec.record(wire::raw("workflow_start", serde_json::json!({
            "workflow_id": workflow_id.clone(),
            "name": wf.name.clone(),
            "yaml_path": yaml_path.clone(),
            "args": args.clone(),
        })))?;
        let node_ids: Vec<&str> = wf.nodes.iter().map(|n| n.id.as_str()).collect();
        let progress = init_progress(&workflow_id, &wf.name, &node_ids);
        write_progress(&gait_dir, &session_id, &progress)?;
        repos.traces.commit_session(&gait_dir, &session_id, "workflow: start")?;

        if let Some(hooks) = &wf.hooks {
            if let Some(cmd) = &hooks.before {
                let env = make_env_vars(&workflow_id, &session_id, &gait_dir, &codebase, &args_str, None, &state);
                let (exit, _) = run_hook(cmd, &env).await;
                rec.record(wire::raw("hook_run", serde_json::json!({
                    "workflow_id": workflow_id.clone(), "node_id": null,
                    "hook_type": "before", "cmd": cmd.clone(), "exit_code": exit,
                })))?;
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
            rec.record(wire::raw("node_skip", serde_json::json!({
                "workflow_id": workflow_id.clone(), "node_id": node_id.clone(),
                "reason": "inactive branch path",
            })))?;
            state.set_status(&node_id, NodeStatus::Skipped);
            update_progress(&gait_dir, &session_id, &workflow_id, &wf.name, &state, &wf.nodes)?;
            repos.traces.commit_session(&gait_dir, &session_id, &format!("workflow: skip {}", node_id))?;
            continue;
        }

        let env = make_env_vars(&workflow_id, &session_id, &gait_dir, &codebase, &args_str, Some(&node_id), &state);
        if let Some(hooks) = &node.hooks {
            if let Some(cmd) = &hooks.before {
                let (exit, _) = run_hook(cmd, &env).await;
                rec.record(wire::raw("hook_run", serde_json::json!({
                    "workflow_id": workflow_id.clone(), "node_id": Some(node_id.clone()),
                    "hook_type": "before", "cmd": cmd.clone(), "exit_code": exit,
                })))?;
            }
        }

        rec.record(wire::raw("node_start", serde_json::json!({
            "workflow_id": workflow_id.clone(), "node_id": node_id.clone(),
            "node_type": node.node_type_name(), "attempt": 1,
        })))?;
        state.set_status(&node_id, NodeStatus::Running);

        let vars = VarContext {
            workflow_id: workflow_id.clone(), session_id: session_id.clone(),
            gait_dir: gait_dir.clone(), codebase: codebase.clone(),
            args: args_str.clone(), node_id: node_id.clone(),
            node_outputs: state.node_outputs.clone(),
            loop_iter: None, loop_max: None, loop_output: None,
            loop_signal: None, node_exit: None, failed_node: state.failed_node.clone(),
        };
        let ctx = NodeContext { workflow_id: &workflow_id, session_id: &session_id, gait_dir: &gait_dir, codebase: &codebase, vars };

        let retry_max = node.retry.as_ref().map(|r| r.max).unwrap_or(1);
        let retry_delay = node.retry.as_ref().map(|r| r.delay).unwrap_or(0);
        let mut node_result: Option<NodeOutput> = None;
        let mut last_error = String::new();

        for attempt in 1..=(retry_max as usize) {
            if attempt > 1 {
                tokio::time::sleep(std::time::Duration::from_millis(retry_delay)).await;
            }
            let result = execute_node(node, &ctx, &wf, provider.clone(), &rec, &workflow_id, &session_id, &gait_dir).await;
            match result {
                Ok(out) => { node_result = Some(out); break; }
                Err(e) => {
                    last_error = e.to_string();
                    rec.record(wire::raw("node_error", serde_json::json!({
                        "workflow_id": workflow_id.clone(), "node_id": node_id.clone(),
                        "error": last_error.clone(), "attempt": attempt as u32,
                    })))?;
                }
            }
        }

        match node_result {
            Some(out) => {
                if out.routed_to.as_deref() == Some("__gate_paused__") {
                    state.set_status(&node_id, NodeStatus::Paused);
                    update_progress(&gait_dir, &session_id, &workflow_id, &wf.name, &state, &wf.nodes)?;
                    repos.traces.commit_session(&gait_dir, &session_id, &format!("workflow: gate {}", node_id))?;
                    eprintln!("[gait workflow] paused at gate '{}'", node_id);
                    return Ok(Outcome::GatePaused);
                }

                if node.branch.is_some() {
                    let routed = out.routed_to.clone().unwrap_or_default();
                    propagate_branch_skips(node, &routed, &wf.nodes, &mut state, &rec, &workflow_id)?;
                }

                rec.record(wire::raw("node_complete", serde_json::json!({
                    "workflow_id": workflow_id.clone(), "node_id": node_id.clone(),
                    "output": out.text.clone(), "exit_code": out.exit_code,
                    "json_output": out.json.clone(), "routed_to": out.routed_to.clone(),
                })))?;
                state.set_status(&node_id, NodeStatus::Done);
                state.node_outputs.insert(node_id.clone(), out);

                if let Some(hooks) = &node.hooks {
                    if let Some(cmd) = &hooks.after {
                        let env = make_env_vars(&workflow_id, &session_id, &gait_dir, &codebase, &args_str, Some(&node_id), &state);
                        let (exit, _) = run_hook(cmd, &env).await;
                        rec.record(wire::raw("hook_run", serde_json::json!({
                            "workflow_id": workflow_id.clone(), "node_id": Some(node_id.clone()),
                            "hook_type": "after", "cmd": cmd.clone(), "exit_code": exit,
                        })))?;
                    }
                }
            }
            None => {
                if let Some(hooks) = &node.hooks {
                    if let Some(cmd) = &hooks.error {
                        let env = make_env_vars(&workflow_id, &session_id, &gait_dir, &codebase, &args_str, Some(&node_id), &state);
                        run_hook(cmd, &env).await;
                    }
                }
                state.set_status(&node_id, NodeStatus::Error(last_error.clone()));

                let on_error = &node.on_error;
                if on_error.is_abort() {
                    rec.record(wire::raw("workflow_error", serde_json::json!({
                        "workflow_id": workflow_id.clone(), "failed_node": node_id.clone(),
                        "error": last_error.clone(),
                    })))?;
                    update_progress(&gait_dir, &session_id, &workflow_id, &wf.name, &state, &wf.nodes)?;
                    repos.traces.commit_session(&gait_dir, &session_id, &format!("workflow: error at {}", node_id))?;
                    if let Some(hooks) = &wf.hooks {
                        if let Some(cmd) = &hooks.error {
                            let mut env = make_env_vars(&workflow_id, &session_id, &gait_dir, &codebase, &args_str, None, &state);
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

        update_progress(&gait_dir, &session_id, &workflow_id, &wf.name, &state, &wf.nodes)?;
        repos.traces.commit_session(&gait_dir, &session_id, &format!("workflow: {}", node_id))?;
    }

    rec.record(wire::raw("workflow_complete", serde_json::json!({
        "workflow_id": workflow_id.clone(),
    })))?;
    update_progress_done(&gait_dir, &session_id, &workflow_id, &wf.name, &state, &wf.nodes)?;
    repos.traces.commit_session(&gait_dir, &session_id, "workflow: complete")?;

    if let Some(hooks) = &wf.hooks {
        if let Some(cmd) = &hooks.after {
        let env = make_env_vars(&workflow_id, &session_id, &gait_dir, &codebase, &args_str, None, &state);
            run_hook(cmd, &env).await;
        }
    }

    eprintln!("[gait workflow] complete: {}", wf.name);
    Ok(Outcome::Completed)
    }.await;

    drop(rec);                                      // close the channel → consumer's drive() returns
    let _failed = consumer.await.unwrap_or(false);  // flush stderr before returning/exiting
    // `_failed` stays unused on purpose: a workflow error already propagates as Err
    // (bail!) → bin/gait maps it to exit(1). A node_error with on_error: continue must
    // NOT fail the run, so we do not route `failed` to the exit code.
    match run_result? {                             // `?` propagates a bail! AFTER draining
        Outcome::GatePaused => std::process::exit(0),
        Outcome::Completed => Ok(()),
    }
}

async fn execute_node(
    node: &NodeDef,
    ctx: &NodeContext<'_>,
    wf: &WorkflowDef,
    provider: Arc<dyn Provider>,
    rec: &crate::proto::Recorder,
    workflow_id: &str,
    session_id: &str,
    gait_dir: &Path,
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
        return exec_gate_node(workflow_id, &node.id, gate_cfg, &ctx.vars, rec);
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
    rec: &crate::proto::Recorder,
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
            rec.record(wire::raw("node_skip", serde_json::json!({
                "workflow_id": workflow_id.to_string(), "node_id": id.clone(),
                "reason": format!("inactive branch (routed to {})", routed_to),
            })))?;
        }
    }
    let _ = all_nodes;
    Ok(())
}

fn update_progress(gait_dir: &Path, session_id: &str, workflow_id: &str, name: &str, state: &WorkflowState, nodes: &[NodeDef]) -> anyhow::Result<()> {
    let pf = build_progress(workflow_id, name, "running", state, nodes);
    write_progress(gait_dir, session_id, &pf)
}

fn update_progress_done(gait_dir: &Path, session_id: &str, workflow_id: &str, name: &str, state: &WorkflowState, nodes: &[NodeDef]) -> anyhow::Result<()> {
    let pf = build_progress(workflow_id, name, "done", state, nodes);
    write_progress(gait_dir, session_id, &pf)
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

fn make_env_vars(workflow_id: &str, session_id: &str, gait_dir: &Path, codebase: &Path, args: &str, node_id: Option<&str>, state: &WorkflowState) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("WORKFLOW_ID".into(), workflow_id.to_string());
    env.insert("SESSION_ID".into(), session_id.to_string());
    env.insert("WORKSPACE".into(), gait_dir.to_string_lossy().to_string());
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
