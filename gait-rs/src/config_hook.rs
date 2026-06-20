//! Shell-command hook implementation. Reads `HookDef` entries from `gait.jsonc`
//! and dispatches them to the appropriate lifecycle point via `sh -c`.
//!
//! All six lifecycle events fire shell commands and record a `hook_run` proto event
//! with the full `HookEnv` snapshot — enabling replay and plugin debugging.
//!
//! Sync methods use `tokio::task::block_in_place` to run the async `run_hook`
//! from within an async context (valid because the caller is always inside
//! Tokio's multi-thread runtime via `run_turn`).

use crate::config::HookDef;
use crate::hook_runner::{build_env, run_hook};
use crate::proto::events;
use crate::types::{Hook, HookAction, HookContext, Message};

pub struct ConfigHook {
    def: HookDef,
}

impl ConfigHook {
    pub fn new(def: HookDef) -> Self {
        Self { def }
    }

    fn matches_event(&self, ctx: &HookContext) -> bool {
        self.def.event == ctx.event
    }

    fn matches_tool(&self, ctx: &HookContext) -> bool {
        match &self.def.tool {
            Some(filter) => *filter == ctx.tool_name,
            None => true,
        }
    }

    /// Run the hook command synchronously via block_in_place.
    fn run_blocking(&self, ctx: &HookContext) -> (i32, String) {
        let env = build_env(ctx);
        let cmd = self.def.run.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(run_hook(&cmd, &env))
        })
    }

    /// Record a hook_run proto event. Non-fatal — failures are silently dropped.
    fn record(
        &self,
        phase: &str,
        exit_code: i32,
        output: &str,
        ctx: &HookContext,
        rec: &crate::Recorder,
    ) {
        let _ = rec.record(events::hook_run(
            phase,
            &self.def.run,
            exit_code,
            output,
            ctx,
        ));
    }
}

#[async_trait::async_trait]
impl Hook for ConfigHook {
    fn pre_turn(&self, ctx: &HookContext, rec: &crate::Recorder) -> Option<String> {
        if !self.matches_event(ctx) {
            return None;
        }
        let (exit, output) = self.run_blocking(ctx);
        self.record("pre_turn", exit, &output, ctx, rec);
        if exit == 0 && !output.trim().is_empty() {
            Some(output)
        } else {
            None
        }
    }

    fn pre_step(
        &self,
        ctx: &HookContext,
        rec: &crate::Recorder,
        _messages: &[Message],
    ) -> Option<Vec<crate::types::MessageMutation>> {
        if !self.matches_event(ctx) {
            return None;
        }
        let (exit, output) = self.run_blocking(ctx);
        self.record("pre_step", exit, &output, ctx, rec);
        // Shell hooks observe pre_step context via GAIT_* env vars but cannot
        // return structured MessageMutations — that requires Rust-side Hook impls.
        // This is intentional: shell hooks at pre_step are observation-only.
        None
    }

    fn post_step(&self, ctx: &HookContext, rec: &crate::Recorder) {
        if !self.matches_event(ctx) {
            return;
        }
        let (exit, output) = self.run_blocking(ctx);
        self.record("post_step", exit, &output, ctx, rec);
    }

    fn pre_tool(&self, ctx: &HookContext, rec: &crate::Recorder) -> HookAction {
        if !self.matches_event(ctx) || !self.matches_tool(ctx) {
            return HookAction::Continue;
        }
        let (exit, output) = self.run_blocking(ctx);
        self.record("pre_tool", exit, &output, ctx, rec);
        if exit != 0 {
            HookAction::Block { reason: output }
        } else {
            HookAction::Continue
        }
    }

    async fn post_tool(&self, ctx: &HookContext, rec: &crate::Recorder) -> Option<String> {
        if !self.matches_event(ctx) || !self.matches_tool(ctx) {
            return None;
        }
        let env = build_env(ctx);
        let (exit_code, output) = run_hook(&self.def.run, &env).await;
        self.record("post_tool", exit_code, &output, ctx, rec);
        if exit_code != 0 {
            Some(format!(
                "{}\n[hook {} failed (exit {exit_code})]\n{output}",
                ctx.tool_result, self.def.run
            ))
        } else if !output.trim().is_empty() {
            Some(output)
        } else {
            None
        }
    }

    fn post_turn(&self, ctx: &HookContext, rec: &crate::Recorder) {
        if !self.matches_event(ctx) {
            return;
        }
        let (exit, output) = self.run_blocking(ctx);
        self.record("post_turn", exit, &output, ctx, rec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Hook;
    use std::path::PathBuf;

    fn hook_def(event: &str, tool: Option<&str>, run: &str) -> HookDef {
        HookDef {
            event: event.into(),
            tool: tool.map(str::to_string),
            run: run.into(),
        }
    }

    fn ctx_for(
        event: &str,
        tool_name: &str,
        tool_result: &str,
        args: serde_json::Value,
    ) -> HookContext {
        HookContext {
            event: event.into(),
            tool_name: tool_name.into(),
            tool_call_id: "call-1".into(),
            tool_result: tool_result.into(),
            tool_args: args,
            gait_dir: PathBuf::from("/tmp"),
            codebase: PathBuf::from("/tmp"),
            ..Default::default()
        }
    }

    fn rec() -> crate::Recorder {
        crate::Recorder::detached()
    }

    // ── post_tool ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn post_tool_success_empty_stdout_returns_none() {
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "exit 0"));
        let ctx = ctx_for("post_tool", "edit", "original", serde_json::json!({}));
        assert!(hook.post_tool(&ctx, &rec()).await.is_none());
    }

    #[tokio::test]
    async fn post_tool_success_nonempty_stdout_replaces_result() {
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "echo 'replaced'"));
        let ctx = ctx_for("post_tool", "edit", "original", serde_json::json!({}));
        let result = hook.post_tool(&ctx, &rec()).await.expect("Some expected");
        assert!(result.contains("replaced"), "got: {result}");
        assert!(
            !result.contains("original"),
            "must not keep old result: {result}"
        );
    }

    #[tokio::test]
    async fn post_tool_failure_annotates_original() {
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "echo 'bad'; exit 1"));
        let ctx = ctx_for("post_tool", "edit", "original", serde_json::json!({}));
        let result = hook.post_tool(&ctx, &rec()).await.expect("Some expected");
        assert!(
            result.contains("original"),
            "must preserve original: {result}"
        );
        assert!(
            result.contains("exit 1"),
            "must include exit code: {result}"
        );
    }

    #[tokio::test]
    async fn post_tool_non_matching_tool_skips() {
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "exit 1"));
        let ctx = ctx_for("post_tool", "shell", "original", serde_json::json!({}));
        assert!(hook.post_tool(&ctx, &rec()).await.is_none());
    }

    #[tokio::test]
    async fn post_tool_wrong_event_skips() {
        let hook = ConfigHook::new(hook_def("pre_tool", Some("edit"), "exit 1"));
        let ctx = ctx_for("post_tool", "edit", "original", serde_json::json!({}));
        assert!(hook.post_tool(&ctx, &rec()).await.is_none());
    }

    #[tokio::test]
    async fn post_tool_args_in_env() {
        let hook = ConfigHook::new(hook_def(
            "post_tool",
            Some("edit"),
            "[ -n \"$GAIT_ARG_FILE_PATH\" ] || exit 1",
        ));
        let ctx = ctx_for(
            "post_tool",
            "edit",
            "original",
            serde_json::json!({"file_path": "src/main.rs"}),
        );
        assert!(
            hook.post_tool(&ctx, &rec()).await.is_none(),
            "GAIT_ARG_FILE_PATH must be set"
        );
    }

    #[tokio::test]
    async fn post_tool_publishes_hook_run_event() {
        use crate::proto::gait::log::v1::{event, payload, stream_item};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sessions").join("h")).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rec = crate::Recorder::for_session(dir.path(), "h", tx);
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "exit 0"));
        let ctx = ctx_for("post_tool", "edit", "orig", serde_json::json!({}));
        let _ = hook.post_tool(&ctx, &rec).await;
        drop(rec);
        let mut saw = false;
        while let Some(item) = rx.recv().await {
            if let Some(stream_item::Item::Event(ev)) = item.item {
                if let Some(event::Body::Payload(p)) = ev.body {
                    if matches!(p.kind, Some(payload::Kind::HookRun(_))) {
                        saw = true;
                    }
                }
            }
        }
        assert!(saw, "post_tool must publish hook_run event");
    }

    // ── pre_tool ─────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn pre_tool_exit_zero_continues() {
        let hook = ConfigHook::new(hook_def("pre_tool", Some("bash"), "exit 0"));
        let ctx = ctx_for("pre_tool", "bash", "", serde_json::json!({}));
        assert_eq!(hook.pre_tool(&ctx, &rec()), HookAction::Continue);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pre_tool_nonzero_blocks_with_reason() {
        let hook = ConfigHook::new(hook_def(
            "pre_tool",
            Some("bash"),
            "echo 'not allowed'; exit 1",
        ));
        let ctx = ctx_for("pre_tool", "bash", "", serde_json::json!({}));
        match hook.pre_tool(&ctx, &rec()) {
            HookAction::Block { reason } => {
                assert!(reason.contains("not allowed"), "got: {reason}")
            }
            HookAction::Continue => panic!("expected Block"),
        }
    }

    #[tokio::test]
    async fn pre_tool_non_matching_tool_continues() {
        let hook = ConfigHook::new(hook_def("pre_tool", Some("bash"), "exit 1"));
        let ctx = ctx_for("pre_tool", "read", "", serde_json::json!({}));
        assert_eq!(hook.pre_tool(&ctx, &rec()), HookAction::Continue);
    }

    // ── pre_turn ─────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn pre_turn_empty_stdout_returns_none() {
        let hook = ConfigHook::new(hook_def("pre_turn", None, "exit 0"));
        let ctx = HookContext {
            event: "pre_turn".into(),
            system_prompt: "original".into(),
            ..Default::default()
        };
        assert!(hook.pre_turn(&ctx, &rec()).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pre_turn_nonempty_stdout_replaces_system_prompt() {
        let hook = ConfigHook::new(hook_def("pre_turn", None, "echo 'new prompt'"));
        let ctx = HookContext {
            event: "pre_turn".into(),
            system_prompt: "original".into(),
            ..Default::default()
        };
        let result = hook.pre_turn(&ctx, &rec()).expect("Some expected");
        assert!(result.contains("new prompt"), "got: {result}");
    }

    #[tokio::test]
    async fn pre_turn_wrong_event_skips() {
        let hook = ConfigHook::new(hook_def("post_turn", None, "echo hi"));
        let ctx = HookContext {
            event: "pre_turn".into(),
            ..Default::default()
        };
        assert!(hook.pre_turn(&ctx, &rec()).is_none());
    }

    // ── post_turn ─────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn post_turn_fires_without_panic() {
        let hook = ConfigHook::new(hook_def("post_turn", None, "exit 0"));
        let ctx = HookContext {
            event: "post_turn".into(),
            turn_outcome: "done".into(),
            ..Default::default()
        };
        hook.post_turn(&ctx, &rec());
    }

    #[tokio::test]
    async fn post_turn_wrong_event_skips() {
        let hook = ConfigHook::new(hook_def("pre_turn", None, "exit 1"));
        let ctx = HookContext {
            event: "post_turn".into(),
            turn_outcome: "done".into(),
            ..Default::default()
        };
        hook.post_turn(&ctx, &rec()); // must not panic
    }
}
