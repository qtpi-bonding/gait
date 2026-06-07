use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub auto: bool,
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
    pub nodes: Vec<NodeDef>,
}

fn default_version() -> String { "1".into() }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksConfig {
    pub before: Option<String>,
    pub after: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default = "default_retry_max")]
    pub max: u32,
    #[serde(default = "default_retry_delay")]
    pub delay: u64,
    #[serde(default = "default_backoff")]
    pub backoff: String,
}

fn default_retry_max() -> u32 { 3 }
fn default_retry_delay() -> u64 { 1000 }
fn default_backoff() -> String { "linear".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OnError {
    Goto { goto: String },
    Named(String),
}

impl Default for OnError {
    fn default() -> Self { Self::Named("abort".into()) }
}

impl OnError {
    pub fn is_abort(&self) -> bool {
        matches!(self, Self::Named(s) if s == "abort")
    }
    pub fn is_continue(&self) -> bool {
        matches!(self, Self::Named(s) if s == "continue")
    }
    pub fn goto_node(&self) -> Option<&str> {
        match self {
            Self::Goto { goto } => Some(goto.as_str()),
            Self::Named(s) if s.starts_with("goto: ") => Some(&s[6..]),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeDef {
    pub id: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub on_error: OnError,
    #[serde(default)]
    pub retry: Option<RetryConfig>,
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
    #[serde(default)]
    pub model: Option<String>,
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
    #[serde(rename = "loop", default)]
    pub loop_node: Option<LoopConfig>,
    #[serde(default)]
    pub branch: Option<BranchConfig>,
    #[serde(default)]
    pub gate: Option<GateConfig>,
}

impl NodeDef {
    pub fn node_type_name(&self) -> &'static str {
        if self.shell.is_some() { "shell" }
        else if self.prompt.is_some() { "prompt" }
        else if self.loop_node.is_some() { "loop" }
        else if self.branch.is_some() { "branch" }
        else if self.gate.is_some() { "gate" }
        else { "unknown" }
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
        match self { Self::Short(s) => s, Self::Full(f) => &f.run }
    }
    pub fn cwd(&self) -> &str {
        match self { Self::Short(_) => "codebase", Self::Full(f) => &f.cwd }
    }
    pub fn timeout_ms(&self) -> u64 {
        match self { Self::Short(_) => 120_000, Self::Full(f) => f.timeout }
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

fn default_cwd() -> String { "codebase".into() }
fn default_shell_timeout() -> u64 { 120_000 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopConfig {
    pub prompt: String,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub shell_check: Option<String>,
    #[serde(default = "default_loop_max")]
    pub max: u32,
    #[serde(default)]
    pub fresh_context: bool,
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
}

fn default_loop_max() -> u32 { 10 }

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
        match self { Self::Short(s) => s, Self::Full { message } => message }
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
    fn parse_on_error_goto() {
        let yaml = r#"
name: err-wf
nodes:
  - id: n
    prompt: "do it"
    on_error:
      goto: cleanup
"#;
        let wf: WorkflowDef = serde_yaml::from_str(yaml).unwrap();
        let on_err = &wf.nodes[0].on_error;
        assert_eq!(on_err.goto_node(), Some("cleanup"));
    }
}
