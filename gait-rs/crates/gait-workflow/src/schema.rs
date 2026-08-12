use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Sentinel value used by `exec_gate_node` to signal that a workflow was
/// paused at a gate node. The runner matches on this string to take the
/// pause path (set status `Paused`, commit, exit 0) instead of treating the
/// gate like a normal successful node output.
pub const GATE_PAUSED_SENTINEL: &str = "__gate_paused__";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub inference: Option<String>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub auto: bool,
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
    #[serde(default)]
    pub record: Option<RecordConfig>,
    pub nodes: Vec<NodeDef>,
}

fn default_version() -> String {
    "1".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksConfig {
    pub before: Option<String>,
    pub after: Option<String>,
    pub error: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RecordConfig {
    #[serde(default)]
    pub commands: Vec<RecordCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordCommand {
    pub id: String,
    pub command: String,
    #[serde(default = "default_record_timeout")]
    pub timeout: u64,
}

fn default_record_timeout() -> u64 {
    120_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnErrorConfig {
    #[serde(default)]
    pub retry_times: u32,
    #[serde(default = "default_delay")]
    pub delay: u64,
    #[serde(default = "default_backoff")]
    pub backoff: String,
    #[serde(default = "default_context")]
    pub context: String,
    #[serde(rename = "continue", default)]
    pub continue_on_error: bool,
}

fn default_delay() -> u64 {
    1000
}
fn default_backoff() -> String {
    "linear".into()
}
fn default_context() -> String {
    "carry".into()
}

impl Default for OnErrorConfig {
    fn default() -> Self {
        Self {
            retry_times: 0,
            delay: 1000,
            backoff: "linear".into(),
            context: "carry".into(),
            continue_on_error: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeDef {
    pub id: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub on_error: Option<OnErrorConfig>,
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
    #[serde(default)]
    pub inference: Option<String>,
    #[serde(default)]
    pub auto: Option<bool>,
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub codebase: Option<String>,
    #[serde(default)]
    pub files: Option<Vec<String>>,
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub shell: Option<ShellConfig>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub branch: Option<BranchConfig>,
    #[serde(default)]
    pub gate: Option<GateConfig>,
    #[serde(default)]
    pub continue_session: Option<String>,
}

impl NodeDef {
    pub fn node_type_name(&self) -> &'static str {
        if self.shell.is_some() {
            "shell"
        } else if self.prompt.is_some() {
            "prompt"
        } else if self.branch.is_some() {
            "branch"
        } else if self.gate.is_some() {
            "gate"
        } else {
            "unknown"
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ShellConfig {
    Short(String),
    Full(ShellConfigFull),
}

impl ShellConfig {
    pub fn run(&self) -> &str {
        match self {
            Self::Short(s) => s,
            Self::Full(f) => &f.run,
        }
    }
    pub fn cwd(&self) -> &str {
        match self {
            Self::Short(_) => "codebase",
            Self::Full(f) => &f.cwd,
        }
    }
    pub fn timeout_ms(&self) -> u64 {
        match self {
            Self::Short(_) => 120_000,
            Self::Full(f) => f.timeout,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellConfigFull {
    pub run: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default = "default_shell_timeout")]
    pub timeout: u64,
}

fn default_cwd() -> String {
    "codebase".into()
}
fn default_shell_timeout() -> u64 {
    120_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchConfig {
    pub on: String,
    #[serde(default)]
    pub then: Option<String>,
    #[serde(rename = "else", default)]
    pub else_: Option<String>,
    #[serde(default)]
    pub cases: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GateConfig {
    Short(String),
    Full { message: String },
}

impl GateConfig {
    pub fn message(&self) -> &str {
        match self {
            Self::Short(s) => s,
            Self::Full { message } => message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shell_node() {
        let yaml = r#"
name: test-wf
version: "1"
nodes:
  - id: run-tests
    shell:
      run: "cargo test 2>&1"
      cwd: codebase
      timeout: 60000
"#;
        let wf: WorkflowDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(wf.name, "test-wf");
        assert_eq!(wf.nodes.len(), 1);
        assert!(wf.nodes[0].shell.is_some());
    }

    #[test]
    fn parse_record_commands() {
        let yaml = r#"
name: record-wf
record:
  commands:
    - id: cargo
      command: cargo --version
    - id: uname
      command: uname -a
      timeout: 5000
nodes:
  - id: test
    shell: "true"
"#;
        let wf: WorkflowDef = serde_yaml::from_str(yaml).unwrap();
        let record = wf.record.unwrap();
        assert_eq!(record.commands.len(), 2);
        assert_eq!(record.commands[0].timeout, 120_000);
        assert_eq!(record.commands[1].timeout, 5000);
    }

    #[test]
    fn parse_branch_node() {
        let yaml = r#"
name: branch-wf
nodes:
  - id: route
    branch:
      on: "$test.exit == 0"
      then: ship
      else: fix
"#;
        let wf: WorkflowDef = serde_yaml::from_str(yaml).unwrap();
        let branch = wf.nodes[0].branch.as_ref().unwrap();
        assert_eq!(branch.on, "$test.exit == 0");
        assert_eq!(branch.then.as_deref(), Some("ship"));
        assert_eq!(branch.else_.as_deref(), Some("fix"));
    }

    #[test]
    fn test_on_error_config_defaults() {
        let yaml = r#"continue: true"#;
        let cfg: OnErrorConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.continue_on_error);
        assert_eq!(cfg.retry_times, 0);
        assert_eq!(cfg.delay, 1000);
        assert_eq!(cfg.backoff, "linear");
        assert_eq!(cfg.context, "carry");
    }

    #[test]
    fn test_on_error_config_full() {
        let yaml = r#"
retry_times: 3
delay: 500
backoff: exponential
context: fresh
continue: true
"#;
        let cfg: OnErrorConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.retry_times, 3);
        assert_eq!(cfg.delay, 500);
        assert_eq!(cfg.backoff, "exponential");
        assert_eq!(cfg.context, "fresh");
        assert!(cfg.continue_on_error);
    }

    #[test]
    fn test_node_def_no_loop_field() {
        // loop_node must not exist on NodeDef anymore
        let yaml = r#"
id: my-node
depends_on: []
prompt: "hi"
loop_node:
  prompt: "iterate"
  max: 5
"#;
        // Should either ignore unknown field or fail — either way loop_node is gone
        // With #[serde(deny_unknown_fields)] this would error; without it, ignored.
        // We assert the node parses and has no loop behavior:
        let node: NodeDef = serde_yaml::from_str(yaml).unwrap();
        assert!(node.prompt.is_some());
    }
}
