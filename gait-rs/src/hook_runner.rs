//! Shared shell-hook execution and context-to-env conversion.
//! The canonical `run_hook` — `config_hook.rs` uses this.
//! Note: `gait-workflow/src/hooks.rs` has a duplicate; consolidating requires a new
//! shared utility crate (tracked as tech-debt).

use std::collections::HashMap;
#[cfg(test)]
use std::path::PathBuf;

use crate::types::HookContext;

/// Execute a shell command via `sh -c` with the given environment.
/// Returns (exit_code, combined_stdout_stderr). Never panics — returns (-1, "") on spawn error.
pub(crate) async fn run_hook(cmd: &str, env: &HashMap<String, String>) -> (i32, String) {
    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .envs(env)
        .output()
        .await
    {
        Ok(o) => {
            let exit = o.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            let out = if stderr.is_empty() {
                stdout
            } else {
                format!("{stdout}{stderr}")
            };
            (exit, out)
        }
        Err(_) => (-1, String::new()),
    }
}

/// Build the full GAIT_* env-var set from a `HookContext`.
/// Every hook invocation gets every variable — unset fields appear as empty strings or "0".
/// Tool args are expanded as `GAIT_ARG_<KEY>` (key uppercased, hyphens → underscores).
pub(crate) fn build_env(ctx: &HookContext) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("GAIT_EVENT".into(), ctx.event.clone());
    env.insert("GAIT_SESSION_ID".into(), ctx.session_id.clone());
    env.insert("GAIT_DIR".into(), ctx.gait_dir.display().to_string());
    env.insert("GAIT_CODEBASE".into(), ctx.codebase.display().to_string());
    env.insert("GAIT_SYSTEM_PROMPT".into(), ctx.system_prompt.clone());
    env.insert("GAIT_STEP_INDEX".into(), ctx.step_index.to_string());
    env.insert("GAIT_STEP_COUNT".into(), ctx.step_count.to_string());
    env.insert("GAIT_MESSAGE_COUNT".into(), ctx.message_count.to_string());
    env.insert("GAIT_LAST_ROLE".into(), ctx.last_role.clone());
    env.insert("GAIT_RESPONSE_TEXT".into(), ctx.response_text.clone());
    env.insert("GAIT_TOOL_COUNT".into(), ctx.tool_count.to_string());
    env.insert("GAIT_TOOL_NAMES".into(), ctx.tool_names.join(","));
    env.insert("GAIT_TOOL_NAME".into(), ctx.tool_name.clone());
    env.insert("GAIT_TOOL_CALL_ID".into(), ctx.tool_call_id.clone());
    env.insert("GAIT_TOOL_RESULT".into(), ctx.tool_result.clone());
    env.insert("GAIT_TURN_OUTCOME".into(), ctx.turn_outcome.clone());
    env.insert("GAIT_FINAL_RESPONSE".into(), ctx.final_response.clone());
    if let Some(obj) = ctx.tool_args.as_object() {
        for (k, v) in obj {
            let key = format!("GAIT_ARG_{}", k.to_uppercase().replace('-', "_"));
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            env.insert(key, val);
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(event: &str) -> HookContext {
        HookContext {
            event: event.into(),
            session_id: "test-session".into(),
            gait_dir: PathBuf::from("/tmp/gait"),
            codebase: PathBuf::from("/tmp/code"),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn run_hook_exit_zero_returns_stdout() {
        let (exit, out) = run_hook("echo hello", &Default::default()).await;
        assert_eq!(exit, 0);
        assert!(out.contains("hello"), "got: {out}");
    }

    #[tokio::test]
    async fn run_hook_nonzero_exit_captured() {
        let (exit, _) = run_hook("exit 2", &Default::default()).await;
        assert_eq!(exit, 2);
    }

    #[tokio::test]
    async fn run_hook_stderr_included_in_output() {
        let (exit, out) = run_hook("echo err >&2", &Default::default()).await;
        assert_eq!(exit, 0);
        assert!(out.contains("err"), "stderr must be included: {out}");
    }

    #[test]
    fn build_env_universal_vars_always_present() {
        let ctx = ctx("pre_turn");
        let env = build_env(&ctx);
        assert_eq!(env.get("GAIT_EVENT").map(String::as_str), Some("pre_turn"));
        assert_eq!(
            env.get("GAIT_SESSION_ID").map(String::as_str),
            Some("test-session")
        );
        assert_eq!(env.get("GAIT_DIR").map(String::as_str), Some("/tmp/gait"));
        assert_eq!(
            env.get("GAIT_CODEBASE").map(String::as_str),
            Some("/tmp/code")
        );
        assert_eq!(env.get("GAIT_STEP_INDEX").map(String::as_str), Some("0"));
        assert_eq!(env.get("GAIT_TOOL_NAME").map(String::as_str), Some(""));
        assert_eq!(env.get("GAIT_TURN_OUTCOME").map(String::as_str), Some(""));
    }

    #[test]
    fn build_env_tool_args_expanded_as_gait_arg() {
        let mut ctx = ctx("pre_tool");
        ctx.tool_args = serde_json::json!({"file_path": "src/main.rs", "count": 5});
        let env = build_env(&ctx);
        assert_eq!(
            env.get("GAIT_ARG_FILE_PATH").map(String::as_str),
            Some("src/main.rs")
        );
        assert_eq!(env.get("GAIT_ARG_COUNT").map(String::as_str), Some("5"));
    }

    #[test]
    fn build_env_tool_args_hyphen_to_underscore() {
        let mut ctx = ctx("pre_tool");
        ctx.tool_args = serde_json::json!({"file-path": "/foo"});
        let env = build_env(&ctx);
        assert_eq!(
            env.get("GAIT_ARG_FILE_PATH").map(String::as_str),
            Some("/foo")
        );
    }

    #[test]
    fn build_env_tool_names_comma_joined() {
        let mut ctx = ctx("post_step");
        ctx.tool_names = vec!["bash".into(), "read".into()];
        let env = build_env(&ctx);
        assert_eq!(
            env.get("GAIT_TOOL_NAMES").map(String::as_str),
            Some("bash,read")
        );
    }
}
