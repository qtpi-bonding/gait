use super::super::*;
use crate::types::ToolDef;

// `pause_for_unlock`/`UnlockPauseCtx` were deleted in Task 9 (plan-01): the
// Unlock pause now goes through `finalize::finalize` like every other exit
// kind. Equivalent coverage lives in
// `finalize::tests::finalize_unlock_emits_event_commits_and_posts_unlock`.

#[test]
fn turn_context_is_wrapped_in_status_tag() {
    let ctx = build_turn_context("deadbeef", &["src/a.rs".to_string()], &[]);
    assert!(ctx.starts_with("<status>"), "got: {ctx}");
    assert!(ctx.trim_end().ends_with("</status>"), "got: {ctx}");
    assert!(ctx.contains("Codebase commit: deadbeef"));
    assert!(ctx.contains("src/a.rs"));
    assert!(
        !ctx.contains("<cwd>"),
        "cwd no longer persists — the status block must not imply a sticky cwd: {ctx}"
    );
}

#[test]
fn build_loadable_catalog_shows_all_kinds() {
    use crate::types::{LoadableEntry, LoadableKind};
    let entries = vec![
        LoadableEntry {
            name: "slack_send".into(),
            kind: LoadableKind::Mcp,
            description: "post to Slack".into(),
            enabled: false,
        },
        LoadableEntry {
            name: "tdd".into(),
            kind: LoadableKind::Skill,
            description: "test-driven workflow".into(),
            enabled: true,
        },
    ];
    let idx = build_loadable_catalog(&entries);
    assert!(idx.contains("<loadable-catalog>"), "got: {idx}");
    assert!(idx.contains("mcp"), "got: {idx}");
    assert!(idx.contains("skill"), "got: {idx}");
    assert!(idx.contains("slack_send"), "got: {idx}");
    assert!(idx.contains("tdd"), "got: {idx}");
    // The instruction line contains "enabled=true/false" as literal help text — that's fine.
    // Per-entry flags would appear as "enabled=true\n" or "enabled=false\n" (at end of entry line).
    // We check that no entry line ends with an enabled= flag.
    for line in idx.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("mcp ")
            || trimmed.starts_with("skill ")
            || trimmed.starts_with("plugin ")
        {
            assert!(
                !trimmed.contains("enabled="),
                "entry line must not contain enabled= flag: {trimmed}"
            );
        }
    }
}

#[test]
fn build_loadable_catalog_empty_emits_none_block() {
    let result = build_loadable_catalog(&[]);
    assert!(result.contains("<loadable-catalog>"), "got: {result}");
    assert!(result.contains("(none)"), "got: {result}");
}

#[test]
fn build_loadable_catalog_includes_plugin_entries() {
    use crate::types::{LoadableEntry, LoadableKind};
    let entries = vec![LoadableEntry {
        name: "myfmt".into(),
        kind: LoadableKind::WasmPlugin,
        description: "wasm plugin (myfmt.wasm)".into(),
        enabled: false,
    }];
    let idx = build_loadable_catalog(&entries);
    assert!(idx.contains("myfmt"), "plugin name in index: {idx}");
    assert!(idx.contains("plugin"), "plugin kind in index: {idx}");
}

#[test]
fn build_permission_status_shows_three_tiers() {
    let allow = vec!["read".to_string(), "shell:ls *".to_string()];
    let deny = vec!["shell:rm *".to_string()];
    let status = build_permission_status(&allow, &deny);
    assert!(status.contains("<permission-status>"), "got: {status}");
    assert!(status.contains("read"), "got: {status}");
    assert!(status.contains("shell:ls *"), "got: {status}");
    assert!(status.contains("shell:rm *"), "got: {status}");
}

#[test]
fn build_permission_status_empty_returns_empty() {
    assert!(build_permission_status(&[], &[]).is_empty());
}

#[test]
fn apply_parse_filter_rg_keeps_matching_lines() {
    let raw = "line1\nFAILED: test_foo\nline3\n";
    let ops = serde_json::json!([{"rg": "FAILED"}]);
    let result = apply_parse_filter(raw, &ops);
    assert!(result.contains("FAILED: test_foo"), "got: {result}");
    assert!(!result.contains("line1"), "got: {result}");
    assert!(!result.contains("line3"), "got: {result}");
}

#[test]
fn apply_parse_filter_sd_replaces_text() {
    let raw = "error: something went wrong\nerror: another thing\n";
    let ops = serde_json::json!([{"sd": ["error: ", "ERR: "]}]);
    let result = apply_parse_filter(raw, &ops);
    assert!(result.contains("ERR: something"), "got: {result}");
    assert!(!result.contains("error: "), "got: {result}");
}

#[test]
fn apply_parse_filter_pipeline_rg_then_sd() {
    let raw = "ok\nFAILED: test_foo\nFAILED: test_bar\n";
    let ops = serde_json::json!([{"rg": "FAILED"}, {"sd": ["FAILED: ", ""]}]);
    let result = apply_parse_filter(raw, &ops);
    assert!(result.contains("test_foo"), "got: {result}");
    assert!(result.contains("test_bar"), "got: {result}");
    assert!(!result.contains("ok"), "got: {result}");
    assert!(!result.contains("FAILED: "), "got: {result}");
}

#[test]
fn apply_parse_filter_rg_no_match_returns_sentinel() {
    let raw = "line1\nline2\n";
    let ops = serde_json::json!([{"rg": "NOTFOUND"}]);
    let result = apply_parse_filter(raw, &ops);
    assert!(
        result.trim() == "(no matches)" || result.trim().is_empty(),
        "got: {result}"
    );
}

#[test]
fn apply_parse_filter_empty_ops_returns_raw() {
    let raw = "hello world";
    let ops = serde_json::json!([]);
    let result = apply_parse_filter(raw, &ops);
    assert_eq!(result, raw);
}

#[test]
fn tool_gating_filters_when_tools_nonempty() {
    use std::collections::HashSet;

    let read = ToolDef {
        name: "read".into(),
        description: "read a file".into(),
        parameters: serde_json::json!({ "type": "object" }),
        execute: Box::new(|_args| {
            Box::pin(async move { Ok(serde_json::Value::String("contents".into())) })
        }),
        permission: None,
    };
    let shell = ToolDef {
        name: "shell".into(),
        description: "run a command".into(),
        parameters: serde_json::json!({ "type": "object" }),
        execute: Box::new(|_args| {
            Box::pin(async move { Ok(serde_json::Value::String("ok".into())) })
        }),
        permission: None,
    };
    let edit = ToolDef {
        name: "edit".into(),
        description: "edit a file".into(),
        parameters: serde_json::json!({ "type": "object" }),
        execute: Box::new(|_args| {
            Box::pin(async move { Ok(serde_json::Value::String("ok".into())) })
        }),
        permission: None,
    };

    let mut tools: Vec<Box<dyn Tool>> = vec![Box::new(read), Box::new(shell), Box::new(edit)];

    // Non-empty enable-set: gate filters down to whitelisted tools only.
    let allowed: HashSet<String> = ["read", "edit"].iter().map(|s| s.to_string()).collect();
    tools.retain(|t| allowed.contains(t.name()));
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        tools.len(),
        2,
        "shell should be filtered out; got: {:?}",
        names
    );
    assert!(
        names.contains(&"read"),
        "read should pass; got: {:?}",
        names
    );
    assert!(
        names.contains(&"edit"),
        "edit should pass; got: {:?}",
        names
    );
    assert!(
        !names.contains(&"shell"),
        "shell should be filtered; got: {:?}",
        names
    );
}

// ── exec_replace_tests: Task 2 regression test ────────────────────────────────
//
// Verifies that `apply_output_filter` emits a `Replace` event targeting the
// `log_event_id` of the original `ToolResult` event when the parse filter
// changes the in-memory result vs. the raw `ctx.tool_result`.
//
// Wired up against real `wire::append` (to seed the log) and a real
// `Recorder` (so `emitter.record` actually writes the Replace). The Replace
// is detected by re-reading the log file — avoids relying on the channel's
// timing/drop semantics.
#[cfg(test)]
mod exec_replace_tests {
    use super::super::super::exec::{apply_output_filter, ToolExecResult};
    use crate::proto::gait::log::v1::{event, payload};
    use crate::proto::{events, wire, Recorder};
    use crate::types::{AssistantPart, HookContext, ToolCall, ToolResult, ToolResultKind};
    use std::sync::Arc;
    use tempfile::TempDir;

    // Local minimal provider — never invoked, only the type is required by
    // `apply_output_filter`'s signature. Avoids the privacy of
    // `crate::proto::events::tests::DummyProvider` (non-`pub` mod).
    use crate::types::Message;
    use async_trait::async_trait;
    use gait_provider::provider::{Provider, ProviderError, StreamChunk};

    struct TestProvider;
    #[async_trait]
    impl Provider for TestProvider {
        fn model_id(&self) -> &str {
            "test"
        }
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[Box<dyn crate::types::Tool>],
            _params: &crate::config::LlmParamsConfig,
            _tail_context: Option<&str>,
        ) -> Result<
            std::pin::Pin<
                Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>,
            >,
            ProviderError,
        > {
            Err(ProviderError::Other("unused".into()))
        }
    }

    #[tokio::test]
    async fn apply_output_filter_emits_replace_when_parse_changes_result() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");

        // Pre-write a raw ToolResult to the log (simulating execute_tool_call).
        let raw_event = wire::append(
            &log_path,
            events::tool_result("cid-1".into(), "line1\nline2\nline3".into(), None, None),
        )
        .unwrap();

        // Real Recorder writing to the same log file.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rec = Recorder::new(log_path.clone(), tx);

        let mut exec = ToolExecResult {
            part: AssistantPart::ToolCall(ToolCall {
                tool_call_id: "cid-1".into(),
                tool_name: "read".into(),
                args: serde_json::json!({}),
            }),
            result: ToolResult {
                kind: ToolResultKind::ToolResult,
                tool_call_id: "cid-1".into(),
                result: serde_json::Value::String("line1\nline2\nline3".into()),
            },
            log_event_id: raw_event.id.clone(),
            denied: None,
        };
        let tc = ToolCall {
            tool_call_id: "cid-1".into(),
            tool_name: "read".into(),
            args: serde_json::json!({"parse": [{"rg": "line2"}]}),
        };
        let ctx = HookContext {
            tool_result: "line1\nline2\nline3".into(),
            tool_call_id: "cid-1".into(),
            ..Default::default()
        };

        let prov: Arc<dyn Provider> = Arc::new(TestProvider);
        apply_output_filter(&mut exec, &tc, &[], &rec, &ctx, "dummy", prov).await;

        // In-memory result is filtered.
        assert_eq!(exec.result.result.as_str().unwrap(), "line2");

        // A Replace event must be in the log, targeting the original ToolResult.
        let events_after = wire::read_events(&log_path).unwrap();
        let mut saw_replace = false;
        for ev in &events_after {
            if let Some(event::Body::Replace(r)) = &ev.body {
                assert_eq!(
                    r.target, raw_event.id,
                    "Replace.target must point at the original ToolResult event"
                );
                let rep = r.replacement.as_ref().expect("replacement set");
                if let Some(payload::Kind::ToolResult(tr)) = &rep.kind {
                    assert_eq!(
                        tr.output, "line2",
                        "Replace payload must carry the filtered output"
                    );
                    assert_eq!(tr.call_id, "cid-1");
                    saw_replace = true;
                }
            }
        }
        assert!(
            saw_replace,
            "Replace event must be emitted when parse filter changes the result"
        );
    }
}

// ── GitSnapshot unit tests ────────────────────────────────────────────────────

#[test]
fn git_snapshot_default_is_empty() {
    let snap = GitSnapshot::default();
    assert!(snap.head_hash.is_empty());
    assert!(snap.dirty_files.is_empty());
}

#[test]
fn turn_input_initial_git_some_carries_seeded_hash() {
    let snap = GitSnapshot {
        head_hash: "seeded-abc123".into(),
        dirty_files: vec!["src/foo.rs".into()],
    };
    let input = TurnInput {
        auto: true,
        max_iter: None,
        preview_policy: crate::config::PreviewPolicy::default(),
        cancel: None,
        sandbox_on: false,
        sandbox_cfg: crate::config::SandboxConfig::default(),
        relay: None,
        initial_git: Some(snap.clone()),
        daemons: vec![],
    };
    let git = input.initial_git.expect("initial_git should be Some");
    assert_eq!(git.head_hash, "seeded-abc123");
    assert_eq!(git.dirty_files, vec!["src/foo.rs"]);
}

// ── check_own_tool_scope disk-free tests ────────────────────────────────────

/// Minimal Tool that reports configurable scope_targets.
struct ScopeTestTool {
    access: crate::types::ToolAccess,
    targets: Vec<(std::path::PathBuf, crate::types::Dimension)>,
}

#[async_trait::async_trait]
impl crate::types::Tool for ScopeTestTool {
    fn name(&self) -> &str {
        "test_tool"
    }
    fn description(&self) -> &str {
        "test"
    }
    fn parameters(&self) -> &serde_json::Value {
        &serde_json::Value::Null
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _rec: &dyn crate::types::Rec,
    ) -> Result<serde_json::Value, crate::tools::ToolError> {
        Ok(serde_json::Value::Null)
    }
    fn access(&self) -> crate::types::ToolAccess {
        self.access
    }
    fn scope_targets(
        &self,
        _args: &serde_json::Value,
    ) -> Vec<(std::path::PathBuf, crate::types::Dimension)> {
        self.targets.clone()
    }
}

#[test]
fn check_own_tool_scope_in_codebase_is_in_scope() {
    // Target path inside codebase must be in-scope (no disk access needed).
    use crate::config::SandboxConfig;
    use crate::types::{Dimension, ToolAccess, ToolCall};

    let tc = ToolCall {
        tool_call_id: "t1".into(),
        tool_name: "test_tool".into(),
        args: serde_json::Value::Null,
    };
    let tool = ScopeTestTool {
        access: ToolAccess {
            reads: true,
            ..Default::default()
        },
        // Deliberately use a non-existent path — disk-free check must still work
        targets: vec![(
            std::path::PathBuf::from("/codebase/src/foo.rs"),
            Dimension::Read,
        )],
    };
    let cfg = SandboxConfig::default();
    let items = check_own_tool_scope(&tc, &tool, std::path::Path::new("/codebase"), &cfg).unwrap();
    assert!(
        items.is_empty(),
        "in-codebase target must be in-scope, got: {items:?}"
    );
}

#[test]
fn check_own_tool_scope_outside_codebase_is_out_of_scope() {
    use crate::config::SandboxConfig;
    use crate::types::{Dimension, ToolAccess, ToolCall};

    let tc = ToolCall {
        tool_call_id: "t2".into(),
        tool_name: "test_tool".into(),
        args: serde_json::Value::Null,
    };
    let tool = ScopeTestTool {
        access: ToolAccess {
            reads: true,
            writes: true,
            ..Default::default()
        },
        targets: vec![(std::path::PathBuf::from("/etc/passwd"), Dimension::Write)],
    };
    let cfg = SandboxConfig::default();
    let items = check_own_tool_scope(&tc, &tool, std::path::Path::new("/codebase"), &cfg).unwrap();
    assert_eq!(
        items.len(),
        1,
        "out-of-scope target must generate an unlock item"
    );
    assert_eq!(items[0].resource, "/etc/passwd");
}

#[test]
fn check_own_tool_scope_nonexistent_path_does_not_touch_disk() {
    // This tests the key property: the function must not fail or panic for paths
    // that don't exist. Previously it would canonicalize the parent, which touches
    // disk. Now it's fully lexical.
    use crate::config::SandboxConfig;
    use crate::types::{Dimension, ToolAccess, ToolCall};

    let tc = ToolCall {
        tool_call_id: "t3".into(),
        tool_name: "test_tool".into(),
        args: serde_json::Value::Null,
    };
    // /this/path/does/not/exist on any reasonable test system
    let nonexistent = std::path::PathBuf::from("/this/path/does/not/exist/new_file.rs");
    // Place it inside codebase: /codebase/this/path/does/not/exist/new_file.rs
    let inside_codebase = std::path::PathBuf::from("/codebase/this/does/not/exist.rs");
    let tool = ScopeTestTool {
        access: ToolAccess {
            reads: true,
            writes: true,
            ..Default::default()
        },
        targets: vec![
            (inside_codebase, Dimension::Write),
            (nonexistent, Dimension::Write),
        ],
    };
    let cfg = SandboxConfig::default();
    // Must not panic; should return exactly 1 item (the truly outside path).
    let items = check_own_tool_scope(&tc, &tool, std::path::Path::new("/codebase"), &cfg).unwrap();
    assert_eq!(
        items.len(),
        1,
        "only the out-of-codebase path should be flagged: {items:?}"
    );
}

#[test]
fn check_own_tool_scope_lexical_dotdot_in_codebase_path() {
    // A path with .. that resolves inside codebase must be treated as in-scope.
    use crate::config::SandboxConfig;
    use crate::types::{Dimension, ToolAccess, ToolCall};

    let tc = ToolCall {
        tool_call_id: "t4".into(),
        tool_name: "test_tool".into(),
        args: serde_json::Value::Null,
    };
    // /codebase/src/../lib.rs normalizes to /codebase/lib.rs — in scope
    let tricky = std::path::PathBuf::from("/codebase/src/../lib.rs");
    let tool = ScopeTestTool {
        access: ToolAccess {
            reads: true,
            ..Default::default()
        },
        targets: vec![(tricky, Dimension::Read)],
    };
    let cfg = SandboxConfig::default();
    let items = check_own_tool_scope(&tc, &tool, std::path::Path::new("/codebase"), &cfg).unwrap();
    assert!(
        items.is_empty(),
        "lexically in-scope path with .. must not be flagged: {items:?}"
    );
}
