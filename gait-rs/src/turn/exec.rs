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
    /// UUID of the `ToolResult` event written to the log. Empty if no event
    /// was written (error path via `error_tool_result`).
    pub log_event_id: String,
}

/// Execute one tool call: emit ToolCall event, run the tool, emit ToolResult event.
/// Used identically by the normal execution loop and the permission-resume path.
/// Does NOT handle output/mcp/skill special tools — those are never gated.
pub(super) async fn execute_tool_call(
    tc: &ToolCall,
    tools: &[Box<dyn Tool>],
    emitter: &Recorder,
    approval_prefix: Option<String>,
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
    // and process exit/timed_out live in `ToolResult.process`. We measure the full
    // tool.execute() call so the timing matches what the model observed.
    let (raw, duration, process) = match tools.iter().find(|t| t.name() == tc.tool_name) {
        Some(t) => {
            let started = std::time::Instant::now();
            let res = t.execute(tc.args.clone(), emitter).await;
            let elapsed = started.elapsed();
            let duration = Some(pbjson_types::Duration {
                seconds: elapsed.as_secs() as i64,
                nanos: elapsed.subsec_nanos() as i32,
            });
            match res {
                Ok(v) => {
                    // Process-tool shape (set by shell today): unwrap to model-facing
                    // text and lift process metadata. The model never sees the JSON
                    // wrapper — turn.rs is the single place that bridges Value → ToolResult.
                    if let (Some(output), Some(exit_code), Some(timed_out)) =
                        (v.get("output"), v.get("exit_code"), v.get("timed_out"))
                    {
                        let output = output.as_str().unwrap_or("").to_string();
                        let exit_code = exit_code.as_i64().unwrap_or(0) as i32;
                        let timed_out = timed_out.as_bool().unwrap_or(false);
                        (
                            output,
                            duration,
                            Some(pb::ProcessResult {
                                exit_code,
                                timed_out,
                            }),
                        )
                    } else if v.is_string() {
                        (
                            v.as_str()
                                .expect("guarded by v.is_string() check above")
                                .to_owned(),
                            duration,
                            None,
                        )
                    } else {
                        (v.to_string(), duration, None)
                    }
                }
                Err(e) => (format!("error: {e}"), duration, None),
            }
        }
        None => (format!("unknown tool: {}", tc.tool_name), None, None),
    };
    let output = match approval_prefix {
        Some(prefix) => format!("{}\n{}", prefix, raw),
        None => raw,
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

/// Build a `ToolExecResult` representing a tool-call error WITHOUT emitting events.
/// Use when `execute_tool_call` fails — the model still gets a result back so the
/// turn can continue. `log_event_id` is empty because no event was written; the
/// filter will skip the Replace on the empty ID.
pub(super) fn error_tool_result(tc: &ToolCall, e: anyhow::Error) -> ToolExecResult {
    let output = format!("error: {e}");
    ToolExecResult {
        part: AssistantPart::ToolCall(tc.clone()),
        result: ToolResult {
            kind: ToolResultKind::ToolResult,
            tool_call_id: tc.tool_call_id.clone(),
            result: serde_json::Value::String(output),
        },
        log_event_id: String::new(),
    }
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
        eprintln!("[gait] parse-filter: {}/{} lines kept", out_lines, in_lines);
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
    provider: Arc<dyn crate::provider::Provider>,
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
        if new_output != ctx.tool_result {
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
}
