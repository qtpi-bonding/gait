//! Single-tool execution helpers and the in-memory post-tool output filter.
//! Owns the bridge from `ToolCall` → `Tool` execution → `ToolResult`, plus
//! the `parse` / `curate_prompt` / hook-based rewrite that happens before the
//! model sees the result on the next turn.

use std::sync::Arc;

use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, Sink, SinkMatch};

use crate::config::PreviewPolicy;
use crate::proto::gait::log::v1 as pb;
use crate::proto::{events, Recorder};
use crate::types::{AssistantPart, Hook, HookContext, Tool, ToolCall, ToolResult, ToolResultKind};

use super::curate::apply_curate_filter;

/// Output of one tool execution: the parts for the in-memory message, the
/// in-memory result, and the log event ID so a Replace can target it later.
pub(super) struct ToolExecResult {
    pub part: AssistantPart,
    pub result: ToolResult,
    /// UUID of the `ToolResult` event written to the log.
    pub log_event_id: String,
    /// Complete, unfiltered tool output retained outside the prompt/event log.
    /// Synthetic results have no full output because no tool output was produced.
    pub full_output: Option<crate::tools::FullOutput>,
    /// Set when the hand detected a kernel-level permission denial (EACCES /
    /// EPERM) from a sealed `shell` command. The turn loop checks this in
    /// supervised mode and calls `pause_for_unlock`; in auto mode it is ignored
    /// (the tool failure result propagates normally). Phase 2c Task 6.
    pub denied: Option<pb::SandboxUnlockItem>,
}

/// Execute one tool call: emit ToolCall event, run the tool, emit ToolResult event.
/// Used identically by the normal execution loop and the permission-resume path.
/// Does NOT handle output/mcp/skill special tools — those are never gated.
///
/// `relay`: when `Some`, the brain is running under a spine — use `RelaySynapse`
/// (fires the tool through the spine over the Nerve). When `None`, falls back to
/// `SubprocessSynapse` (direct spawn of `gait __run-tool`, Phase 2).
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_tool_call(
    tc: &ToolCall,
    tools: &[Arc<dyn Tool>],
    emitter: &Recorder,
    approval_prefix: Option<String>,
    _codebase: &std::path::Path,
    gait_dir: &std::path::Path,
    session_id: &str,
    _sandbox_on: bool,
    relay: Option<&crate::turn::nerve::NerveWriter>,
) -> anyhow::Result<ToolExecResult> {
    let id = tc.tool_call_id.clone();
    emitter.record(events::tool_call(
        id.clone(),
        tc.tool_name.clone(),
        &tc.args,
    ))?;
    let assistant_part = AssistantPart::ToolCall(ToolCall {
        tool_call_id: id.clone(),
        tool_name: tc.tool_name.clone(),
        args: tc.args.clone(),
    });
    // Generic timing for every tool. Shell no longer bakes "exit N (Xms)" into
    // its output string — duration lives in the proto `ToolResult.duration` field
    // and process exit live in `ToolResult.process`. We measure the full
    // tool.execute() call so the timing matches what the model observed.
    //
    // `synapse_denied` carries a seal-denial signal from the hand (Phase 2c Task 6):
    // set when the hand detects EACCES/EPERM in a `shell` command's output while
    // running under Seatbelt. The turn loop inspects it after execute_tool_call returns.

    let (raw, duration, process, synapse_denied, full_output) = match tools
        .iter()
        .find(|t| t.name() == tc.tool_name)
    {
        Some(t) => {
            // Schema-guided coercion (integer/number/boolean string decoding + the
            // unconditional float->int pass it layers on top of) needs the tool's
            // parameters schema, so it lives here, not in the pre-lookup site. The
            // `None` (unknown-tool) arm never uses `coerced_args` so nothing else
            // changes. `coerce_int_params` is idempotent so re-applying it inside
            // `coerce_scalar_params` is harmless.
            let coerced_args = crate::tools::coerce_scalar_params(tc.args.clone(), t.parameters());
            let started = std::time::Instant::now();
            // `denied` is extracted only on the world-tool (synapse) path; it stays
            // None on the brain path (control/transcript tools never touch the kernel).
            let is_relay_spawn =
                matches!(t.name(), "spawn_agent" | "run_workflow") && relay.is_some();
            let via_hand = t.access().touches_world() || is_relay_spawn;
            let (res, denied) = if via_hand {
                // Hand path: run via the synapse, then replay the returned events
                // into the real log (between ToolCall and ToolResult).
                // Fall back to InProcessSynapse when not running as the `gait` binary
                // (test binaries have different names and lack the `__run-tool` subcommand).
                // The subprocess path is covered end-to-end by gait-bin/tests/run_tool_hand.rs.
                use crate::turn::synapse::{ScopeRequest, Synapse as _};
                let scope_req = ScopeRequest { access: t.access() };
                let fired = if let Some(nerve_writer) = relay {
                    // Phase 3: brain is under a spine — delegate via Nerve.
                    use crate::turn::synapse::RelaySynapse;
                    RelaySynapse {
                        writer: nerve_writer.clone(),
                    }
                    .fire(t.as_ref(), coerced_args.clone(), scope_req)
                    .await
                } else {
                    // No spine (tests, direct cargo test) — run in-process.
                    // SubprocessSynapse was removed; the production path always
                    // has a relay handle. The in-process path covers tests.
                    use crate::turn::synapse::InProcessSynapse;
                    InProcessSynapse
                        .fire(t.as_ref(), coerced_args.clone(), scope_req)
                        .await
                };
                match fired {
                    Ok((value, events, denied_item, whitelist_items, sandbox_denied_item)) => {
                        for event in events {
                            if let Some(body) = event.body {
                                emitter.record(body)?;
                            }
                        }
                        if !whitelist_items.is_empty() {
                            emitter.record(events::sandbox_whitelist_match(&whitelist_items))?;
                        }
                        if let Some(sd) = &sandbox_denied_item {
                            emitter.record(events::sandbox_denied(
                                &sd.tool_name,
                                &sd.resource,
                                &sd.dimension,
                                &sd.matched_signature,
                                sd.hint.clone(),
                            ))?;
                        }
                        (Ok(value), denied_item)
                    }
                    Err(e) => (Err(e), None),
                }
            } else {
                // Brain path: control/transcript tool, runs in-process as before.
                (t.execute(coerced_args.clone(), emitter).await, None)
            };
            let elapsed = started.elapsed();
            let duration = Some(pbjson_types::Duration {
                seconds: elapsed.as_secs() as i64,
                nanos: elapsed.subsec_nanos() as i32,
            });
            match res {
                Ok(v) => {
                    let (v, full_output) = if via_hand && relay.is_some() {
                        let full_output =
                            crate::tools::full_output_ref_from_value(&v, gait_dir, session_id)
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "relayed tool result missing a valid full-output reference"
                                    )
                                })?;
                        (v, Some(full_output))
                    } else if relay.is_none() {
                        let (v, full_output) = crate::tools::ensure_archived_model_value(
                            v,
                            gait_dir,
                            session_id,
                            &tc.tool_name,
                        )?;
                        (v, Some(full_output))
                    } else {
                        // Control/transcript/MCP tools intentionally execute in
                        // the brain. Send their completed value to the spine so
                        // every successful tool still crosses the same full-output
                        // archive and model-facing boundary.
                        let v = crate::turn::synapse::RelaySynapse {
                            writer: relay.expect("guarded by relay.is_some()").clone(),
                        }
                        .archive_output(&tc.tool_name, v)
                        .await?;
                        let full_output =
                            crate::tools::full_output_ref_from_value(&v, gait_dir, session_id)
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                    "archived tool result missing a valid full-output reference"
                                )
                                })?;
                        (v, Some(full_output))
                    };
                    // Process-tool shape (set by shell today): unwrap to model-facing
                    // text and lift process metadata. The model never sees the JSON
                    // wrapper — turn.rs is the single place that bridges Value → ToolResult.
                    if let (Some(output), Some(exit_code)) = (v.get("output"), v.get("exit_code")) {
                        let output = output.as_str().unwrap_or("").to_string();
                        let exit_code = exit_code.as_i64().unwrap_or(0) as i32;
                        (
                            output,
                            duration,
                            Some(pb::ProcessResult { exit_code }),
                            denied,
                            full_output,
                        )
                    } else if v.is_string() {
                        (
                            v.as_str()
                                .expect("guarded by v.is_string() check above")
                                .to_owned(),
                            duration,
                            None,
                            denied,
                            full_output,
                        )
                    } else {
                        (v.to_string(), duration, None, denied, full_output)
                    }
                }
                Err(e) => {
                    let value = serde_json::Value::String(format!("error: {e}"));
                    let (value, full_output) = if relay.is_none() {
                        let (value, full_output) = crate::tools::ensure_archived_model_value(
                            value,
                            gait_dir,
                            session_id,
                            &tc.tool_name,
                        )?;
                        (value, Some(full_output))
                    } else {
                        // Relay failures are transport failures, not tool output.
                        (value, None)
                    };
                    (
                        value.as_str().unwrap_or_default().to_string(),
                        duration,
                        None,
                        denied,
                        full_output,
                    )
                }
            }
        }
        None => {
            let value = serde_json::Value::String(format!("unknown tool: {}", tc.tool_name));
            let (value, full_output) = if relay.is_none() {
                let (value, full_output) = crate::tools::ensure_archived_model_value(
                    value,
                    gait_dir,
                    session_id,
                    &tc.tool_name,
                )?;
                (value, Some(full_output))
            } else {
                (value, None)
            };
            (
                value.as_str().unwrap_or_default().to_string(),
                None,
                None,
                None,
                full_output,
            )
        }
    };
    let output = match (approval_prefix, full_output.as_ref()) {
        (Some(prefix), Some(full_output)) => {
            crate::tools::model_text_for_inline_with_note(&raw, &prefix, full_output)
        }
        (Some(prefix), None) => format!("{prefix}\n{raw}"),
        (None, _) => raw,
    };
    let tr_event = emitter.record(events::tool_result(
        id.clone(),
        output.clone(),
        duration,
        process,
    ))?;
    let tool_result = ToolResult {
        kind: ToolResultKind::ToolResult,
        tool_call_id: id,
        result: serde_json::Value::String(output),
    };
    Ok(ToolExecResult {
        part: assistant_part,
        result: tool_result,
        log_event_id: tr_event.id,
        full_output,
        denied: synapse_denied,
    })
}

/// In `always` mode, stream a tool's preview (e.g. a diff) live on the ephemeral
/// `delta` channel — never written to the log. No-op for other policies or when
/// the tool has no preview.
pub(super) fn stream_preview_if_always(
    policy: PreviewPolicy,
    tool: Option<&dyn Tool>,
    args: &serde_json::Value,
    emitter: &Recorder,
) {
    if policy != PreviewPolicy::Always {
        return;
    }
    if let Some(p) = tool.and_then(|t| t.preview(args)) {
        emitter.publish_delta(format!("\n{}\n", p));
    }
}

/// Build and durably record a `ToolExecResult` for a tool-call error.
///
/// `execute_tool_call` records the `ToolCall` before work begins. The next model
/// request is reconstructed from the durable log, so every later failure must
/// record the matching `ToolResult` here or the provider sees an orphaned call.
pub(super) fn error_tool_result(
    tc: &ToolCall,
    e: anyhow::Error,
    emitter: &Recorder,
) -> anyhow::Result<ToolExecResult> {
    let output = format!("error: {e}");
    let event = emitter.record(events::tool_result(
        tc.tool_call_id.clone(),
        output.clone(),
        None,
        None,
    ))?;
    Ok(ToolExecResult {
        part: AssistantPart::ToolCall(tc.clone()),
        result: ToolResult {
            kind: ToolResultKind::ToolResult,
            tool_call_id: tc.tool_call_id.clone(),
            result: serde_json::Value::String(output),
        },
        log_event_id: event.id,
        full_output: None,
        denied: None,
    })
}

/// Run a pipeline of rg (grep) and sd-style (find/replace) operations on `raw` in-process.
/// `ops` is a JSON array of `{"rg": "<pattern>"}` and/or `{"sd": ["<from>", "<to>"]}` items.
/// No subprocess, no shell injection surface — pure in-process regex, linear time, no backtracking.
///
/// `pub(crate)` because `turn/tests.rs` calls it directly via `use super::*`.
pub(crate) fn apply_parse_filter(raw: &str, ops: &serde_json::Value) -> String {
    let ops = match ops.as_array() {
        Some(a) => a,
        None => return raw.to_string(),
    };
    if ops.is_empty() {
        return raw.to_string();
    }

    let mut current = raw.to_string();

    for op in ops {
        if let Some(pattern) = op.get("rg").and_then(|v| v.as_str()) {
            struct LineCollector(Vec<String>);
            impl Sink for LineCollector {
                type Error = std::io::Error;
                fn matched(
                    &mut self,
                    _searcher: &Searcher,
                    mat: &SinkMatch<'_>,
                ) -> Result<bool, Self::Error> {
                    let line = String::from_utf8_lossy(mat.bytes()).into_owned();
                    self.0.push(line.trim_end_matches('\n').to_string());
                    Ok(true)
                }
            }
            current = match RegexMatcher::new(pattern) {
                Ok(matcher) => {
                    let mut collector = LineCollector(vec![]);
                    let _ =
                        Searcher::new().search_slice(&matcher, current.as_bytes(), &mut collector);
                    if collector.0.is_empty() {
                        "(no matches)".to_string()
                    } else {
                        collector.0.join("\n")
                    }
                }
                Err(e) => format!("[rg error: invalid pattern {:?}: {}]", pattern, e),
            };
        } else if let Some(sd_args) = op.get("sd").and_then(|v| v.as_array()) {
            // Use regex::bytes directly — the `sd` CLI crate is binary-only and has no lib API.
            let from = sd_args.first().and_then(|v| v.as_str()).unwrap_or("");
            let to = sd_args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            current = match regex::bytes::RegexBuilder::new(from)
                .multi_line(true)
                .build()
            {
                Ok(re) => {
                    let replaced = re.replace_all(current.as_bytes(), to.as_bytes());
                    String::from_utf8_lossy(&replaced).into_owned()
                }
                Err(e) => format!("[sd error: invalid pattern {:?}: {}]", from, e),
            };
        }
    }

    let in_lines = raw.lines().count();
    let out_lines = current.lines().count();
    if out_lines != in_lines {
        #[allow(clippy::print_stderr)]
        // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
        {
            eprintln!("[gait] parse-filter: {}/{} lines kept", out_lines, in_lines);
        }
    }
    current
}

/// Apply the post-tool pipeline to a single tool result.
/// Calls `post_tool` hooks first (they may rewrite the result), then runs the
/// in-memory output filter (`parse` regex ops or `curate_prompt` side-turn).
/// Mutates `exec.result` AND, if the output changed, emits a `Replace` event
/// targeting the original `ToolResult` event (`exec.log_event_id`) so retrace
/// reconstructs the filtered (not raw) version on replay.
///
/// Skip the Replace when `log_event_id` is empty (the error path produced no
/// event to target).
pub(super) async fn apply_output_filter(
    exec: &mut ToolExecResult,
    tc: &ToolCall,
    hooks: &[Box<dyn Hook>],
    emitter: &Recorder,
    ctx: &HookContext,
    model: &str,
    provider: Arc<dyn gait_provider::provider::Provider>,
) {
    // Post-tool hooks — ctx.tool_result is set by the caller to the original raw output
    for h in hooks {
        if let Some(new) = h.post_tool(ctx, emitter).await {
            exec.result.result = serde_json::Value::String(new);
        }
    }
    // Parse/curate filter — mutates the in-memory result.
    let result_str = exec
        .result
        .result
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| exec.result.result.to_string());
    if let Some(parse_ops) = tc.args.get("parse") {
        exec.result.result = serde_json::Value::String(apply_parse_filter(&result_str, parse_ops));
    } else if let Some(curate_prompt) = tc.args.get("curate_prompt").and_then(|v| v.as_str()) {
        let gd = ctx.gait_dir.clone();
        let cp = ctx.codebase.clone();
        let m = model.to_string();
        let prov = provider.clone();
        exec.result.result = Box::pin(async move {
            apply_curate_filter(&result_str, curate_prompt, &gd, &cp, &m, prov).await
        })
        .await;
    }

    let filtered = exec
        .result
        .result
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| exec.result.result.to_string());
    if filtered == ctx.tool_result {
        return;
    }
    exec.result.result = serde_json::Value::String(match exec.full_output.as_ref() {
        Some(full_output) => crate::tools::model_text_for_inline(&filtered, full_output),
        None => filtered,
    });

    // If any transform changed the result vs the original raw, emit a Replace so
    // retrace uses the filtered output on replay. Skip if log_event_id is empty
    // (error path) or the result is unchanged.
    if !exec.log_event_id.is_empty() {
        let new_output = exec
            .result
            .result
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| exec.result.result.to_string());
        let replacement = pb::Payload {
            kind: Some(pb::payload::Kind::ToolResult(pb::ToolResult {
                call_id: exec.result.tool_call_id.clone(),
                output: new_output,
                ..Default::default()
            })),
        };
        let _ = emitter.record(crate::proto::events::replace(
            exec.log_event_id.clone(),
            replacement,
            None,
        ));
    }
}

#[cfg(test)]
mod synapse_routing_tests {
    use super::*;
    use crate::types::{Tool, ToolAccess, ToolCall};
    use tempfile::tempdir;

    struct TouchTool;
    #[async_trait::async_trait]
    impl Tool for TouchTool {
        fn name(&self) -> &str {
            "touch"
        }
        fn description(&self) -> &str {
            ""
        }
        fn parameters(&self) -> &serde_json::Value {
            static P: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            P.get_or_init(|| serde_json::json!({"type":"object"}))
        }
        async fn execute(
            &self,
            _a: serde_json::Value,
            rec: &dyn crate::types::Rec,
        ) -> Result<serde_json::Value, crate::tools::ToolError> {
            rec.record(crate::proto::events::cwd("/from-tool")).ok();
            Ok(serde_json::Value::String("ok".into()))
        }
        fn access(&self) -> ToolAccess {
            ToolAccess {
                reads: true,
                ..Default::default()
            }
        }
    }

    struct HugeOutputTool;
    #[async_trait::async_trait]
    impl Tool for HugeOutputTool {
        fn name(&self) -> &str {
            "huge_output"
        }
        fn description(&self) -> &str {
            ""
        }
        fn parameters(&self) -> &serde_json::Value {
            static P: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            P.get_or_init(|| serde_json::json!({"type":"object"}))
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _rec: &dyn crate::types::Rec,
        ) -> Result<serde_json::Value, crate::tools::ToolError> {
            Ok(serde_json::Value::String(
                "line\n".repeat(crate::tools::MODEL_TOOL_OUTPUT_MAX_BYTES),
            ))
        }
    }

    #[tokio::test]
    async fn world_tool_events_are_recorded_by_caller() {
        let dir = tempdir().unwrap();
        // Create session directory so the log file can be written.
        std::fs::create_dir_all(
            crate::session::live::log_path(dir.path(), "stest123")
                .parent()
                .unwrap(),
        )
        .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rec = crate::proto::Recorder::for_session(dir.path(), "stest123", tx);
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(TouchTool)];
        let tc = ToolCall {
            tool_call_id: "c1".into(),
            tool_name: "touch".into(),
            args: serde_json::json!({}),
        };

        let out = execute_tool_call(
            &tc,
            &tools,
            &rec,
            None,
            dir.path(),
            dir.path(),
            "stest123",
            true,
            None, // no relay in test
        )
        .await
        .unwrap();
        let result = out.result.result.as_str().unwrap();
        assert!(result.starts_with("ok\n\n[full output:"));
        assert_eq!(
            std::fs::read_to_string(out.full_output.as_ref().unwrap().path.clone()).unwrap(),
            "ok"
        );

        // The cwd event the tool emitted must be present in the real log.
        let log = std::fs::read_to_string(&rec.tx_path).unwrap();
        assert!(
            log.contains("/from-tool"),
            "tool-emitted event must be replayed into the log"
        );
        let call = log.find("\"toolCall\"").unwrap();
        let cwd = log.find("/from-tool").unwrap();
        let result = log.rfind("\"toolResult\"").unwrap();
        assert!(
            call < cwd && cwd < result,
            "durable event order was not ToolCall -> tool event -> ToolResult"
        );
    }

    struct ErrorTool;
    #[async_trait::async_trait]
    impl Tool for ErrorTool {
        fn name(&self) -> &str {
            "error_tool"
        }
        fn description(&self) -> &str {
            ""
        }
        fn parameters(&self) -> &serde_json::Value {
            static P: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            P.get_or_init(|| serde_json::json!({"type":"object"}))
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _rec: &dyn crate::types::Rec,
        ) -> Result<serde_json::Value, crate::tools::ToolError> {
            Err(crate::tools::ToolError::Execute("probe failed".into()))
        }
    }

    struct NamedTool {
        name: &'static str,
        value: &'static str,
    }
    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            ""
        }
        fn parameters(&self) -> &serde_json::Value {
            static P: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            P.get_or_init(|| serde_json::json!({"type":"object"}))
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _rec: &dyn crate::types::Rec,
        ) -> Result<serde_json::Value, crate::tools::ToolError> {
            Ok(serde_json::Value::String(self.value.into()))
        }
    }

    #[tokio::test]
    async fn baseline_outer_error_metadata_and_unknown_tool_contract() {
        let dir = tempdir().unwrap();
        let id = "scontract1";
        std::fs::create_dir_all(
            crate::session::live::log_path(dir.path(), id)
                .parent()
                .unwrap(),
        )
        .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rec = crate::proto::Recorder::for_session(dir.path(), id, tx);
        let unknown = execute_tool_call(
            &ToolCall {
                tool_call_id: "unknown".into(),
                tool_name: "missing".into(),
                args: serde_json::json!({}),
            },
            &[],
            &rec,
            None,
            dir.path(),
            dir.path(),
            id,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(unknown
            .result
            .result
            .as_str()
            .unwrap()
            .starts_with("unknown tool: missing"));
        let known = execute_tool_call(
            &ToolCall {
                tool_call_id: "error".into(),
                tool_name: "error_tool".into(),
                args: serde_json::json!({}),
            },
            &[Arc::new(ErrorTool)],
            &rec,
            None,
            dir.path(),
            dir.path(),
            id,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(known
            .result
            .result
            .as_str()
            .unwrap()
            .starts_with("error: probe failed"));
        let events = crate::session::log::read_events(&rec.tx_path).unwrap();
        let unknown_result = events
            .iter()
            .find_map(|e| match crate::turn::event_kind(e) {
                Some(crate::proto::gait::log::v1::payload::Kind::ToolResult(r))
                    if r.call_id == "unknown" =>
                {
                    Some(r)
                }
                _ => None,
            })
            .unwrap();
        assert!(unknown_result.duration.is_none() && unknown_result.process.is_none());
        let known_result = events
            .iter()
            .find_map(|e| match crate::turn::event_kind(e) {
                Some(crate::proto::gait::log::v1::payload::Kind::ToolResult(r))
                    if r.call_id == "error" =>
                {
                    Some(r)
                }
                _ => None,
            })
            .unwrap();
        assert!(known_result.duration.is_some() && known_result.process.is_none());
    }

    #[tokio::test]
    async fn baseline_zero_tools_and_duplicate_name_resolution() {
        let dir = tempdir().unwrap();
        let id = "sduplicate1";
        std::fs::create_dir_all(
            crate::session::live::log_path(dir.path(), id)
                .parent()
                .unwrap(),
        )
        .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rec = crate::proto::Recorder::for_session(dir.path(), id, tx);
        let tc = ToolCall {
            tool_call_id: "empty".into(),
            tool_name: "same".into(),
            args: serde_json::json!({}),
        };
        let empty = execute_tool_call(
            &tc,
            &[],
            &rec,
            None,
            dir.path(),
            dir.path(),
            id,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(empty
            .result
            .result
            .as_str()
            .unwrap()
            .starts_with("unknown tool: same"));
        let first: Arc<dyn Tool> = Arc::new(NamedTool {
            name: "same",
            value: "first",
        });
        let second: Arc<dyn Tool> = Arc::new(NamedTool {
            name: "same",
            value: "second",
        });
        let outer = execute_tool_call(
            &tc,
            &[
                Arc::new(NamedTool {
                    name: "same",
                    value: "first",
                }),
                Arc::new(NamedTool {
                    name: "same",
                    value: "second",
                }),
            ],
            &rec,
            None,
            dir.path(),
            dir.path(),
            id,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(outer.result.result.as_str().unwrap().starts_with("first"));
        let dispatcher = crate::turn::capabilities::CapabilityDispatcher::new(
            vec![first, second],
            Vec::new(),
            std::collections::HashMap::new(),
            std::iter::empty::<String>(),
        );
        let nested = dispatcher
            .invoke(
                crate::turn::capabilities::CapabilityInvocation {
                    address: crate::turn::capabilities::CapabilityAddress::Native {
                        tool: "same".into(),
                    },
                    args: serde_json::json!({}),
                },
                &rec,
            )
            .await
            .unwrap();
        assert_eq!(nested, serde_json::json!("second"));
    }

    #[tokio::test]
    async fn shared_output_boundary_applies_before_model_and_log() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(
            crate::session::live::log_path(dir.path(), "sbound1")
                .parent()
                .unwrap(),
        )
        .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rec = crate::proto::Recorder::for_session(dir.path(), "sbound1", tx);
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(HugeOutputTool)];
        let tc = ToolCall {
            tool_call_id: "c-bound".into(),
            tool_name: "huge_output".into(),
            args: serde_json::json!({}),
        };

        let out = execute_tool_call(
            &tc,
            &tools,
            &rec,
            None,
            dir.path(),
            dir.path(),
            "sbound1",
            false,
            None,
        )
        .await
        .unwrap();
        let model_output = out.result.result.as_str().unwrap();
        assert!(model_output.contains("Tool output truncated by Gait"));
        assert!(model_output.contains("[full output:"));
        assert!(model_output.len() <= crate::tools::MODEL_TOOL_OUTPUT_MAX_BYTES);
        assert_eq!(
            std::fs::read_to_string(&out.full_output.as_ref().unwrap().path).unwrap(),
            "line\n".repeat(crate::tools::MODEL_TOOL_OUTPUT_MAX_BYTES)
        );

        let log = std::fs::read_to_string(&rec.tx_path).unwrap();
        assert!(log.contains("Tool output truncated by Gait"));
        assert!(!log.contains(&"line\\n".repeat(crate::tools::MODEL_TOOL_OUTPUT_MAX_BYTES)));
    }

    #[test]
    fn failed_execution_is_durable_and_retraces_as_a_pair() {
        let dir = tempdir().unwrap();
        let session_id = "serror1";
        std::fs::create_dir_all(
            crate::session::live::log_path(dir.path(), session_id)
                .parent()
                .unwrap(),
        )
        .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rec = crate::proto::Recorder::for_session(dir.path(), session_id, tx);
        let tc = ToolCall {
            tool_call_id: "c-error".into(),
            tool_name: "spawn_agent".into(),
            args: serde_json::json!({"task": "check"}),
        };
        rec.record(events::tool_call(
            tc.tool_call_id.clone(),
            tc.tool_name.clone(),
            &tc.args,
        ))
        .unwrap();

        let result = error_tool_result(
            &tc,
            anyhow::anyhow!("relayed result missing its archive reference"),
            &rec,
        )
        .unwrap();
        assert!(!result.log_event_id.is_empty());

        let events = crate::session::log::read_events(&rec.tx_path).unwrap();
        let messages = crate::session::retrace::messages(&events).unwrap();
        let call_is_present = messages.iter().any(|message| {
            matches!(
                message,
                crate::types::Message::Assistant {
                    content: crate::types::AssistantContent::Parts(parts),
                    ..
                } if parts.iter().any(|part| matches!(
                    part,
                    crate::types::AssistantPart::ToolCall(call)
                        if call.tool_call_id == tc.tool_call_id
                ))
            )
        });
        let result_is_present = messages.iter().any(|message| {
            matches!(
                message,
                crate::types::Message::ToolResult { content, .. }
                    if content.iter().any(|item|
                        item.tool_call_id == tc.tool_call_id
                            && item.result.as_str().is_some_and(|text|
                                text.contains("relayed result missing its archive reference")
                            )
                    )
            )
        });
        assert!(
            call_is_present && result_is_present,
            "messages: {messages:?}"
        );
    }
}
