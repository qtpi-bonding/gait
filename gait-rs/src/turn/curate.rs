//! Launches a nested turn via run_turn to run the curate/annotate filter. This is the only recursive caller of run_turn in the codebase.

use std::path::Path;
use std::sync::Arc;

use super::{event_kind, run_turn, TurnInput};
use crate::proto::gait::log::v1::payload;
use crate::proto::Recorder;
use crate::session::log::read_events;
use crate::types::Tool;

const CURATE_SYSTEM_PROMPT: &str = "You are a curator. You have been given content and a curation task. Call annotate() exactly once with the relevant and irrelevant items. Do not call any other tool. Do not respond with text.";

pub(super) async fn apply_curate_filter(
    raw: &str,
    curate_prompt: &str,
    gait_dir: &Path,
    codebase_path: &Path,
    model: &str,
    provider: Arc<dyn crate::provider::Provider>,
) -> serde_json::Value {
    use crate::tools::annotate::AnnotateTool;

    let repos = crate::git::Repos::production();
    let session_id = crate::session::new_session_id();
    let task = format!(
        "Content to curate:\n{}\n\n---\nCuration task: {}",
        raw, curate_prompt
    );

    let session = crate::session::Session {
        id: session_id.clone(),
        gait_dir: gait_dir.to_path_buf(),
        codebase_path: codebase_path.to_path_buf(),
        assigned_by: None,
        repos: repos.clone(),
    };
    let caps = crate::profile::ResolvedProfile {
        provider,
        permissioned: crate::tools::RegisteredTools(vec![AnnotateTool::new().boxed()])
            .into_enabled()
            .with_permissions(vec![], vec![]),
        model: model.to_string(),
        instructions: CURATE_SYSTEM_PROMPT.to_string(),
        mcp_registry: vec![],
        mcp_clients: std::collections::HashMap::new(),
        skill_registry: vec![],
        plugin_registry: vec![],
        config_enabled: std::collections::HashSet::new(),
        output_schema_retries: 3,
        result_schema: None,
        hooks: vec![],
        llm_params: crate::config::LlmParamsConfig::default(),
    };
    // curate is a side turn — transcript only, no live stream consumer.
    let (tx, _rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let curate_emitter = Recorder::for_session(&session.gait_dir, &session.id, tx);
    // Option A: append the curate prompt as the user_turn, then run_turn folds it.
    // The session worktree must exist before the append (run_turn also creates it).
    let _ = repos.traces.create_session_worktree(gait_dir, &session_id);
    // Recording failure is non-fatal: curate continues folding whatever events are already in the session log even if this user_turn append fails.
    for body in crate::embed::user_turn_events(&repos, codebase_path, task) {
        let _ = curate_emitter.record(body);
    }
    let _ = run_turn(
        &session,
        &curate_emitter,
        caps,
        TurnInput {
            auto: true,
            max_iter: Some(1),
            preview_policy: crate::config::PreviewPolicy::default(),
            cancel: None,
        },
    )
    .await;

    let _ = repos.checkpoints.remove(gait_dir, &session_id);

    let tx_path = gait_dir
        .join("sessions")
        .join(&session_id)
        .join("log.jsonl");
    read_events(&tx_path)
        .ok()
        .and_then(|events| {
            events.into_iter().rev().find_map(|e| {
                if let Some(payload::Kind::ToolCall(tc)) = event_kind(&e) {
                    if tc.name == "annotate" {
                        Some(
                            tc.input
                                .as_ref()
                                .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Null))
                                .unwrap_or(serde_json::Value::Null),
                        )
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        })
        .map(|v| {
            let relevant_items: Vec<&str> = v
                .get("relevant")
                .and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(|s| s.as_str()).collect())
                .unwrap_or_default();
            let irrelevant_count = v
                .get("irrelevant")
                .and_then(|r| r.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            eprintln!(
                "[gait] curate-filter: {}/{} items kept",
                relevant_items.len(),
                relevant_items.len() + irrelevant_count
            );
            let body = relevant_items.join("\n");
            if irrelevant_count > 0 {
                serde_json::Value::String(format!(
                    "[curate-filter: {} kept, {} dropped]\n{}",
                    relevant_items.len(),
                    irrelevant_count,
                    body
                ))
            } else {
                serde_json::Value::String(body)
            }
        })
        .unwrap_or_else(|| serde_json::Value::String(raw.to_string()))
}
