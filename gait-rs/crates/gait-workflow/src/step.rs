//! Typed step options (js-workflows task 3): the JSON-facing structs that
//! script/JS workflows author steps against, plus the shared env builder for
//! env-cleared shell steps and hooks.
//!
//! Every struct is strict: unknown fields are a parse error and field names
//! are camelCase. `serde(flatten)` is deliberately not used anywhere — it
//! defeats `deny_unknown_fields` and blurs the boundary between step layers.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Workflow-level config step (the `config`/`workflow` step of a script
/// workflow).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConfigOpts {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub auto: Option<bool>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub record: Option<RecordOpts>,
    /// Cap on executed steps for one run (`maxSteps`). `None` means the host
    /// default applies.
    #[serde(default)]
    pub max_steps: Option<usize>,
}

/// Evidence-record commands, declared at workflow level.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RecordOpts {
    pub commands: Vec<RecordCommandOpts>,
}

/// One developer-declared evidence command.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RecordCommandOpts {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// Retry policy for a step.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RetryOpts {
    pub times: u32,
    #[serde(default)]
    pub delay: u64,
    #[serde(default)]
    pub backoff: Backoff,
}

/// Retry backoff curve.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Backoff {
    #[default]
    Linear,
    Exponential,
}

/// What a step's failure does to the workflow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum OnFail {
    #[default]
    Throw,
    Continue,
}

/// Per-step hooks (workflow-level hooks are dropped per the spec; these stay).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StepHooks {
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A prompt (model turn) step.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TurnOpts {
    pub id: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub auto: Option<bool>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub codebase: Option<String>,
    #[serde(default)]
    pub continue_session: Option<String>,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub retry: Option<RetryOpts>,
    #[serde(default)]
    pub on_fail: Option<OnFail>,
    #[serde(default)]
    pub hooks: Option<StepHooks>,
}

/// A shell step. Either `argv` (executed directly, no shell) or a literal
/// `script` run via `sh -c`; never both, never neither.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ShellOpts {
    pub id: String,
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub retry: Option<RetryOpts>,
    #[serde(default)]
    pub on_fail: Option<OnFail>,
    #[serde(default)]
    pub hooks: Option<StepHooks>,
}

impl ShellOpts {
    /// Validate argv/script exclusivity. Called by `exec_shell_opts` so all
    /// shell-step paths share one check.
    pub fn validate(&self) -> anyhow::Result<()> {
        match (&self.argv, &self.script) {
            (Some(_), Some(_)) => anyhow::bail!(
                "shell step '{}': argv and script are mutually exclusive",
                self.id
            ),
            (None, None) => {
                anyhow::bail!(
                    "shell step '{}': one of argv or script is required",
                    self.id
                )
            }
            _ => Ok(()),
        }
    }
}

/// A gate (human approval) step.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GateOpts {
    pub id: String,
    pub message: String,
}

/// The outcome of one completed step, regardless of step kind.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StepResult {
    pub output: String,
    pub exit: Option<i32>,
    pub json: Option<Value>,
    pub session_id: Option<String>,
    pub error: Option<String>,
}

// ── Environment construction ───────────────────────────────────────────────

/// Environment variables gait passes through from its own environment to a
/// shell step or hook, in addition to the explicit allowlist below. User /
/// config supplied.
pub const DEFAULT_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "SSH_AUTH_SOCK",
    "GH_TOKEN",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "TMPDIR",
    "LANG",
];

/// Error type for `build_shell_env`: a user-supplied env key colliding with a
/// reserved (gait-controlled) name.
#[derive(Debug, thiserror::Error)]
#[error("env key '{0}' is reserved: GAIT_* and the well-known step variables ({1}) are set by gait and cannot be overridden")]
pub struct ReservedEnvKey(pub String, pub &'static str);

/// Build the full environment for a shell step or hook: the child's
/// environment is **cleared** — nothing from gait's own environment leaks
/// (provider keys included) — then the allowlist + passthrough list + the
/// well-known step variables are added. `extra` (a step's own `env` map) is
/// applied last but may never override a well-known var or use a
/// `GAIT_`-prefixed name: both are gait's namespace and a collision is a hard
/// error, never a silent override.
pub fn build_shell_env(
    well_known: &BTreeMap<String, String>,
    extra: &BTreeMap<String, String>,
    passthrough: &[String],
) -> Result<std::collections::HashMap<String, String>, ReservedEnvKey> {
    let mut env = std::collections::HashMap::new();
    // 1. Allowlist: forward from gait's environment only when set. `GAIT_*`
    // is NOT in the allowlist — those names are gait-internal and may carry
    // credentials; a passthrough entry can still name one explicitly.
    for key in DEFAULT_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            env.insert((*key).to_string(), value.to_string_lossy().into_owned());
        }
    }
    // 2. Explicit passthrough list (config supplied).
    for key in passthrough {
        if let Some(value) = std::env::var_os(key) {
            env.insert(key.clone(), value.to_string_lossy().into_owned());
        }
    }
    // 3. Well-known step variables — gait-controlled, override allowlist/passthrough.
    for (k, v) in well_known {
        env.insert(k.clone(), v.clone());
    }
    // 4. Step's own env: never a reserved name.
    for (k, v) in extra {
        if k.starts_with("GAIT_") {
            return Err(ReservedEnvKey(k.clone(), "GAIT_*"));
        }
        if well_known.contains_key(k) {
            return Err(ReservedEnvKey(k.clone(), "well-known step variables"));
        }
        env.insert(k.clone(), v.clone());
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_opts_rejects_argv_and_script_together() {
        let opts = ShellOpts {
            id: "sh".into(),
            argv: Some(vec!["true".into()]),
            script: Some("true".into()),
            env: BTreeMap::new(),
            timeout: None,
            output_schema: None,
            retry: None,
            on_fail: None,
            hooks: None,
        };
        assert!(opts.validate().is_err());
    }

    #[test]
    fn shell_opts_rejects_neither_argv_nor_script() {
        let opts = ShellOpts {
            id: "sh".into(),
            argv: None,
            script: None,
            env: BTreeMap::new(),
            timeout: None,
            output_schema: None,
            retry: None,
            on_fail: None,
            hooks: None,
        };
        assert!(opts.validate().is_err());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = serde_json::from_str::<ShellOpts>(r#"{"id":"sh","script":"true","cwd":"/tmp"}"#)
            .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn build_shell_env_rejects_well_known_collision() {
        let well_known: BTreeMap<String, String> = [("NODE_ID".to_string(), "n1".to_string())]
            .into_iter()
            .collect();
        let extra: BTreeMap<String, String> = [("NODE_ID".to_string(), "evil".to_string())]
            .into_iter()
            .collect();
        assert!(build_shell_env(&well_known, &extra, &[]).is_err());
    }
}
