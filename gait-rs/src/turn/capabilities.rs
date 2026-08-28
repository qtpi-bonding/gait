//! Turn-scoped capability dispatch below the model transcript boundary.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use crate::config::McpToolDef;
use crate::types::{Rec, Tool};
use async_trait::async_trait;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CapabilityAddress {
    Native { tool: String },
    Mcp { server: String, tool: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CapabilityInvocation {
    pub address: CapabilityAddress,
    pub args: Value,
}

#[async_trait]
pub trait McpInvoker: Send + Sync {
    async fn call(&self, tool: &str, args: Value) -> anyhow::Result<Value>;
}

#[async_trait]
impl McpInvoker for crate::mcp::McpClient {
    async fn call(&self, tool: &str, args: Value) -> anyhow::Result<Value> {
        self.call_tool(tool, args).await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    #[error("unknown native capability `{0}`")]
    UnknownNative(String),
    #[error("unknown MCP server `{0}`")]
    UnknownMcpServer(String),
    #[error("MCP server `{0}` is disabled")]
    DisabledMcpServer(String),
    #[error("unknown MCP tool `{server}.{tool}`")]
    UnknownMcpTool { server: String, tool: String },
    #[error("invalid arguments for `{address}`: {message}")]
    InvalidArguments { address: String, message: String },
    #[error("capability `{0}` is reserved")]
    Reserved(String),
}

pub struct CapabilityDispatcher {
    native: HashMap<String, Arc<dyn Tool>>,
    mcp: HashMap<String, Arc<dyn McpInvoker>>,
    registry: Vec<McpToolDef>,
    enabled: RwLock<HashSet<String>>,
}

impl std::fmt::Debug for CapabilityDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityDispatcher")
            .field("native", &self.native.keys().collect::<Vec<_>>())
            .field("mcp", &self.mcp.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl CapabilityDispatcher {
    pub fn new(
        tools: impl IntoIterator<Item = Arc<dyn Tool>>,
        registry: Vec<McpToolDef>,
        clients: HashMap<String, Arc<dyn McpInvoker>>,
        enabled: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            native: tools
                .into_iter()
                .map(|t| (t.name().to_owned(), t))
                .collect(),
            mcp: clients,
            registry,
            enabled: RwLock::new(enabled.into_iter().collect()),
        }
    }
    pub fn set_enabled(&self, server: impl Into<String>, value: bool) {
        let server = server.into();
        let mut enabled = self.enabled.write().expect("dispatcher lock");
        if value {
            enabled.insert(server);
        } else {
            enabled.remove(&server);
        }
    }
    pub fn is_enabled(&self, server: &str) -> bool {
        self.enabled
            .read()
            .expect("dispatcher lock")
            .contains(server)
    }

    pub fn mcp_tools<'a>(&'a self, server: &'a str) -> impl Iterator<Item = &'a McpToolDef> + 'a {
        self.registry
            .iter()
            .filter(move |tool| tool.server_name == server)
    }

    /// Execute without recording ToolCall/ToolResult events. Native tools still
    /// receive the recorder they require for their own durable side effects.
    pub async fn invoke(
        &self,
        invocation: CapabilityInvocation,
        rec: &dyn Rec,
    ) -> anyhow::Result<Value> {
        match invocation.address {
            CapabilityAddress::Native { tool } => {
                // These names are owned by the outer turn protocol. They must
                // never be callable from guest code (doing so would create
                // nested transcript/control events).
                if matches!(
                    tool.as_str(),
                    "code" | "output" | "structured_done" | "clarify" | "inform" | "prune"
                ) {
                    return Err(CapabilityError::Reserved(tool).into());
                }
                let t = self
                    .native
                    .get(&tool)
                    .ok_or_else(|| CapabilityError::UnknownNative(tool.clone()))?;
                let args = crate::tools::coerce_scalar_params(invocation.args, t.parameters());
                t.execute(args, rec)
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
            }
            CapabilityAddress::Mcp { server, tool } => {
                let client = self
                    .mcp
                    .get(&server)
                    .ok_or_else(|| CapabilityError::UnknownMcpServer(server.clone()))?;
                if !self.is_enabled(&server) {
                    return Err(CapabilityError::DisabledMcpServer(server).into());
                }
                let def = self
                    .registry
                    .iter()
                    .find(|d| d.server_name == server && d.name == tool)
                    .ok_or_else(|| CapabilityError::UnknownMcpTool {
                        server: server.clone(),
                        tool: tool.clone(),
                    })?;
                let schema = jsonschema::JSONSchema::compile(&def.schema).map_err(|e| {
                    CapabilityError::InvalidArguments {
                        address: format!("{server}.{tool}"),
                        message: e.to_string(),
                    }
                })?;
                if let Err(errors) = schema.validate(&invocation.args) {
                    let message = errors.map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
                    return Err(CapabilityError::InvalidArguments {
                        address: format!("{server}.{tool}"),
                        message,
                    }
                    .into());
                }
                client.call(&tool, invocation.args).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TouchTool;
    #[async_trait]
    impl Tool for TouchTool {
        fn name(&self) -> &str {
            "touch"
        }
        fn description(&self) -> &str {
            ""
        }
        fn parameters(&self) -> &Value {
            static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            P.get_or_init(|| serde_json::json!({"type":"object"}))
        }
        async fn execute(
            &self,
            _args: Value,
            rec: &dyn Rec,
        ) -> Result<Value, crate::tools::ToolError> {
            rec.record(crate::proto::events::cwd("/from-tool")).unwrap();
            Ok(Value::String("ok".into()))
        }
        fn access(&self) -> crate::types::ToolAccess {
            crate::types::ToolAccess {
                reads: true,
                ..Default::default()
            }
        }
    }

    struct EchoArgsTool;
    #[async_trait]
    impl Tool for EchoArgsTool {
        fn name(&self) -> &str {
            "echo_args"
        }
        fn description(&self) -> &str {
            ""
        }
        fn parameters(&self) -> &Value {
            static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            P.get_or_init(
                || serde_json::json!({"type":"object","properties":{"n":{"type":"integer"}}}),
            )
        }
        async fn execute(
            &self,
            args: Value,
            _rec: &dyn Rec,
        ) -> Result<Value, crate::tools::ToolError> {
            Ok(args)
        }
    }

    fn recorder(dir: &std::path::Path, id: &str) -> crate::proto::Recorder {
        std::fs::create_dir_all(crate::session::live::log_path(dir, id).parent().unwrap()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        crate::proto::Recorder::for_session(dir, id, tx)
    }

    #[tokio::test]
    async fn baseline_code_mode_native_calls_currently_fail_with_unknown_native() {
        let dispatcher = CapabilityDispatcher::new(
            std::iter::empty::<Arc<dyn Tool>>(),
            Vec::new(),
            HashMap::new(),
            std::iter::empty::<String>(),
        );
        let dir = tempfile::tempdir().unwrap();
        let rec = recorder(dir.path(), "sbaseline1");
        let err = dispatcher
            .invoke(
                CapabilityInvocation {
                    address: CapabilityAddress::Native {
                        tool: "read".into(),
                    },
                    args: serde_json::json!({}),
                },
                &rec,
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown native capability `read`");
    }

    #[tokio::test]
    async fn baseline_reserved_names_today() {
        let dispatcher = CapabilityDispatcher::new(
            std::iter::empty::<Arc<dyn Tool>>(),
            Vec::new(),
            HashMap::new(),
            std::iter::empty::<String>(),
        );
        let dir = tempfile::tempdir().unwrap();
        let rec = recorder(dir.path(), "sreserved1");
        for reserved in [
            "code",
            "output",
            "structured_done",
            "clarify",
            "inform",
            "prune",
        ] {
            let err = dispatcher
                .invoke(
                    CapabilityInvocation {
                        address: CapabilityAddress::Native {
                            tool: reserved.into(),
                        },
                        args: serde_json::json!({}),
                    },
                    &rec,
                )
                .await
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("capability `{reserved}` is reserved")
            );
        }
        for name in ["enable", "annotate"] {
            let err = dispatcher
                .invoke(
                    CapabilityInvocation {
                        address: CapabilityAddress::Native { tool: name.into() },
                        args: serde_json::json!({}),
                    },
                    &rec,
                )
                .await
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("unknown native capability `{name}`")
            );
        }
    }

    #[tokio::test]
    async fn baseline_nested_native_call_executes_directly_today() {
        let dispatcher = CapabilityDispatcher::new(
            vec![Arc::new(TouchTool) as Arc<dyn Tool>],
            Vec::new(),
            HashMap::new(),
            std::iter::empty::<String>(),
        );
        let dir = tempfile::tempdir().unwrap();
        let rec = recorder(dir.path(), "sdirect1");
        let result = dispatcher
            .invoke(
                CapabilityInvocation {
                    address: CapabilityAddress::Native {
                        tool: "touch".into(),
                    },
                    args: serde_json::json!({}),
                },
                &rec,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!("ok"));
        let log = std::fs::read_to_string(&rec.tx_path).unwrap();
        assert!(log.contains("/from-tool"));
        assert!(!log.contains("\"toolCall\""));
        assert!(!log.contains("\"toolResult\""));
    }

    #[tokio::test]
    async fn baseline_nested_native_call_coerces_scalar_params_today() {
        let dispatcher = CapabilityDispatcher::new(
            vec![Arc::new(EchoArgsTool) as Arc<dyn Tool>],
            Vec::new(),
            HashMap::new(),
            std::iter::empty::<String>(),
        );
        let dir = tempfile::tempdir().unwrap();
        let rec = recorder(dir.path(), "scoerce1");
        let result = dispatcher
            .invoke(
                CapabilityInvocation {
                    address: CapabilityAddress::Native {
                        tool: "echo_args".into(),
                    },
                    args: serde_json::json!({"n":"3"}),
                },
                &rec,
            )
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({"n": 3}));
    }
}
