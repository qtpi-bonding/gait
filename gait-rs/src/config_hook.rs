use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::HookDef;
use crate::proto::events;

pub struct ConfigHook {
    def: HookDef,
    gait_dir: PathBuf,
    codebase: PathBuf,
}

impl ConfigHook {
    pub fn new(def: HookDef, gait_dir: PathBuf, codebase: PathBuf) -> Self {
        Self { def, gait_dir, codebase }
    }
}

#[async_trait::async_trait]
impl crate::types::Hook for ConfigHook {
    async fn post_tool(
        &self,
        name: &str,
        call_id: &str,
        args: &serde_json::Value,
        result: &str,
        rec: &crate::Recorder,
    ) -> Option<String> {
        if self.def.event != "post_tool" {
            return None;
        }
        if let Some(ref filter) = self.def.tool {
            if filter != name {
                return None;
            }
        }

        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("GAIT_TOOL_NAME".into(), name.to_string());
        env.insert("GAIT_TOOL_RESULT".into(), result.to_string());
        // Path context so a hook can reference user-owned scripts portably,
        // e.g. "$GAIT_DIR/scripts/edit-syntax-check.sh" — no hardcoded paths.
        env.insert("GAIT_DIR".into(), self.gait_dir.display().to_string());
        env.insert("GAIT_CODEBASE".into(), self.codebase.display().to_string());
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let key = format!("GAIT_ARG_{}", k.to_uppercase().replace('-', "_"));
                let val = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                env.insert(key, val);
            }
        }

        let (exit_code, output) = crate::workflow::hooks::run_hook(&self.def.run, &env).await;

        let _ = rec.record(events::hook_run("post_tool", &self.def.run, exit_code, &output, Some(call_id.to_string())));

        if exit_code != 0 {
            Some(format!(
                "{result}\n[hook {} failed (exit {exit_code})]\n{output}",
                self.def.run
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Hook;
    use tempfile::TempDir;

    // Test helper: use the temp dir as both gait_dir and codebase.
    fn gd(dir: &TempDir) -> PathBuf { dir.path().to_path_buf() }

    fn hook_def(event: &str, tool: Option<&str>, run: &str) -> HookDef {
        HookDef {
            event: event.to_string(),
            tool: tool.map(|s| s.to_string()),
            run: run.to_string(),
        }
    }

    #[tokio::test]
    async fn matching_tool_success_returns_none() {
        let dir = TempDir::new().unwrap();
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "exit 0"), gd(&dir), gd(&dir));
        let result = hook.post_tool("edit", "call-1", &serde_json::json!({}), "original", &crate::Recorder::detached()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn matching_tool_failure_appends_output() {
        let dir = TempDir::new().unwrap();
        let hook = ConfigHook::new(
            hook_def("post_tool", Some("edit"), "echo 'syntax error'; exit 1"),
            gd(&dir), gd(&dir),
        );
        let result = hook.post_tool("edit", "call-1", &serde_json::json!({}), "original", &crate::Recorder::detached()).await;
        let new = result.expect("failure must return Some");
        assert!(new.contains("original"), "must preserve original: {new}");
        assert!(new.contains("exit 1"), "must include exit code: {new}");
    }

    #[tokio::test]
    async fn non_matching_tool_skips() {
        let dir = TempDir::new().unwrap();
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "exit 1"), gd(&dir), gd(&dir));
        let result = hook.post_tool("shell", "call-1", &serde_json::json!({}), "original", &crate::Recorder::detached()).await;
        assert!(result.is_none(), "non-matching tool must return None");
    }

    #[tokio::test]
    async fn no_tool_filter_runs_for_all_tools() {
        let dir = TempDir::new().unwrap();
        let hook = ConfigHook::new(hook_def("post_tool", None, "exit 0"), gd(&dir), gd(&dir));
        let result = hook.post_tool("shell", "call-1", &serde_json::json!({}), "original", &crate::Recorder::detached()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn args_exposed_as_env_vars() {
        let dir = TempDir::new().unwrap();
        let hook = ConfigHook::new(
            hook_def("post_tool", Some("edit"), "[ -n \"$GAIT_ARG_FILE_PATH\" ] || exit 1"),
            gd(&dir), gd(&dir),
        );
        let args = serde_json::json!({"file_path": "src/main.rs"});
        let result = hook.post_tool("edit", "call-1", &args, "original", &crate::Recorder::detached()).await;
        assert!(result.is_none(), "GAIT_ARG_FILE_PATH must be set: {result:?}");
    }

    #[tokio::test]
    async fn gait_dir_and_codebase_exposed_as_env_vars() {
        let dir = TempDir::new().unwrap();
        // The hook fails unless BOTH $GAIT_DIR and $GAIT_CODEBASE are set — the
        // contract that lets a hook reference user-owned scripts portably.
        let hook = ConfigHook::new(
            hook_def("post_tool", Some("edit"),
                "[ -n \"$GAIT_DIR\" ] && [ -n \"$GAIT_CODEBASE\" ] || exit 1"),
            gd(&dir), gd(&dir),
        );
        let result = hook.post_tool("edit", "call-1", &serde_json::json!({}), "original", &crate::Recorder::detached()).await;
        assert!(result.is_none(), "GAIT_DIR and GAIT_CODEBASE must be set: {result:?}");
    }

    #[tokio::test]
    async fn wrong_event_type_skips() {
        let dir = TempDir::new().unwrap();
        let hook = ConfigHook::new(hook_def("pre_tool", Some("edit"), "exit 1"), gd(&dir), gd(&dir));
        let result = hook.post_tool("edit", "call-1", &serde_json::json!({}), "original", &crate::Recorder::detached()).await;
        assert!(result.is_none(), "pre_tool hook must not fire in post_tool");
    }

    #[tokio::test]
    async fn post_tool_publishes_hook_run() {
        use crate::proto::gait::log::v1::{stream_item, payload, event};
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sessions").join("h")).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rec = crate::Recorder::for_session(dir.path(), "h", tx);
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "exit 0"), gd(&dir), gd(&dir));
        let _ = hook.post_tool("edit", "c1", &serde_json::json!({}), "orig", &rec).await;
        drop(rec);
        let mut saw = false;
        while let Some(item) = rx.recv().await {
            if let Some(stream_item::Item::Event(ev)) = item.item {
                if let Some(event::Body::Payload(p)) = ev.body {
                    if matches!(p.kind, Some(payload::Kind::HookRun(_))) { saw = true; }
                }
            }
        }
        assert!(saw, "post_tool must publish hook_run");
    }

}
