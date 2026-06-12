use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::HookDef;
use crate::proto::{events, wire};

pub struct ConfigHook {
    def: HookDef,
    tx_path: PathBuf,
}

impl ConfigHook {
    pub fn new(def: HookDef, tx_path: PathBuf) -> Self {
        Self { def, tx_path }
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

        let _ = wire::append(
            &self.tx_path,
            events::hook_run("post_tool", &self.def.run, exit_code, &output, Some(call_id.to_string())),
        );

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
        let tx = dir.path().join("transcript.jsonl");
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "exit 0"), tx);
        let result = hook.post_tool("edit", "call-1", &serde_json::json!({}), "original").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn matching_tool_failure_appends_output() {
        let dir = TempDir::new().unwrap();
        let tx = dir.path().join("transcript.jsonl");
        let hook = ConfigHook::new(
            hook_def("post_tool", Some("edit"), "echo 'syntax error'; exit 1"),
            tx,
        );
        let result = hook.post_tool("edit", "call-1", &serde_json::json!({}), "original").await;
        let new = result.expect("failure must return Some");
        assert!(new.contains("original"), "must preserve original: {new}");
        assert!(new.contains("exit 1"), "must include exit code: {new}");
    }

    #[tokio::test]
    async fn non_matching_tool_skips() {
        let dir = TempDir::new().unwrap();
        let tx = dir.path().join("transcript.jsonl");
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "exit 1"), tx);
        let result = hook.post_tool("shell", "call-1", &serde_json::json!({}), "original").await;
        assert!(result.is_none(), "non-matching tool must return None");
    }

    #[tokio::test]
    async fn no_tool_filter_runs_for_all_tools() {
        let dir = TempDir::new().unwrap();
        let tx = dir.path().join("transcript.jsonl");
        let hook = ConfigHook::new(hook_def("post_tool", None, "exit 0"), tx);
        let result = hook.post_tool("shell", "call-1", &serde_json::json!({}), "original").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn args_exposed_as_env_vars() {
        let dir = TempDir::new().unwrap();
        let tx = dir.path().join("transcript.jsonl");
        let hook = ConfigHook::new(
            hook_def("post_tool", Some("edit"), "[ -n \"$GAIT_ARG_FILE_PATH\" ] || exit 1"),
            tx.clone(),
        );
        let args = serde_json::json!({"file_path": "src/main.rs"});
        let result = hook.post_tool("edit", "call-1", &args, "original").await;
        assert!(result.is_none(), "GAIT_ARG_FILE_PATH must be set: {result:?}");
    }

    #[tokio::test]
    async fn wrong_event_type_skips() {
        let dir = TempDir::new().unwrap();
        let tx = dir.path().join("transcript.jsonl");
        let hook = ConfigHook::new(hook_def("pre_tool", Some("edit"), "exit 1"), tx);
        let result = hook.post_tool("edit", "call-1", &serde_json::json!({}), "original").await;
        assert!(result.is_none(), "pre_tool hook must not fire in post_tool");
    }
}
