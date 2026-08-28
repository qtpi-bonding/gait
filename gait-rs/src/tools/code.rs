//! Model-facing Code Mode evaluator tool.
use crate::turn::capabilities::{CapabilityAddress, CapabilityDispatcher, CapabilityInvocation};
use crate::types::{ExecutionMode, ExecutionSignal, Rec, Tool, ToolError};
use async_trait::async_trait;
use gait_codemode::{ApiSurface, CapabilityHost, Limits};
use serde_json::Value;
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct DispatcherSlot(Arc<RwLock<Option<Arc<CapabilityDispatcher>>>>);

struct Host<'a> {
    slot: DispatcherSlot,
    rec: &'a dyn Rec,
}
#[async_trait]
impl<'a> CapabilityHost for Host<'a> {
    async fn call(
        &self,
        namespace: &str,
        method: &str,
        args: Value,
    ) -> Result<Value, gait_codemode::HostError> {
        let dispatcher = self
            .slot
            .0
            .read()
            .expect("code dispatcher lock")
            .clone()
            .ok_or_else(|| {
                gait_codemode::HostError::Failed(
                    "Code Mode capability dispatcher is unavailable".into(),
                )
            })?;
        let address = if namespace == "gait" {
            CapabilityAddress::Native {
                tool: method.to_owned(),
            }
        } else {
            CapabilityAddress::Mcp {
                server: namespace.to_owned(),
                tool: method.to_owned(),
            }
        };
        dispatcher
            .invoke(CapabilityInvocation { address, args }, self.rec)
            .await
            .map_err(|e| gait_codemode::HostError::Failed(e.to_string()))
    }
}

pub struct CodeTool {
    slot: DispatcherSlot,
    schema: Value,
}
impl Default for CodeTool {
    fn default() -> Self {
        Self::new()
    }
}
impl CodeTool {
    pub fn new() -> Self {
        Self::with_slot(DispatcherSlot(Arc::new(RwLock::new(None))))
    }
    pub fn with_slot(slot: DispatcherSlot) -> Self {
        Self {
            slot,
            schema: serde_json::json!({
                "type":"object", "properties":{"source":{"type":"string"}},
                "required":["source"], "additionalProperties":false
            }),
        }
    }
    pub fn dispatcher_slot(&self) -> Arc<RwLock<Option<Arc<CapabilityDispatcher>>>> {
        self.slot.0.clone()
    }
    pub fn install_dispatcher(&self, dispatcher: Arc<CapabilityDispatcher>) {
        *self.slot.0.write().expect("code dispatcher lock") = Some(dispatcher);
    }
}
#[async_trait]
impl Tool for CodeTool {
    fn name(&self) -> &str {
        "code"
    }
    fn description(&self) -> &str {
        "Evaluate JavaScript Code Mode source with access to enabled capabilities."
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    async fn execute(&self, args: Value, _rec: &dyn Rec) -> Result<Value, ToolError> {
        let obj = args
            .as_object()
            .ok_or_else(|| ToolError::InvalidParams("expected exactly {source:string}".into()))?;
        if obj.len() != 1 || !obj.contains_key("source") {
            return Err(ToolError::InvalidParams(
                "expected exactly {source:string}".into(),
            ));
        }
        let source = obj["source"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParams("source must be a string".into()))?;
        let host = Host {
            slot: self.slot.clone(),
            rec: _rec,
        };
        gait_codemode::run(source, ApiSurface::default(), &host, Limits::default())
            .await
            .map_err(|e| ToolError::Execute(format!("Code Mode evaluation failed: {e}")))
    }
    fn preview(&self, args: &Value) -> Option<String> {
        args.get("source")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Serial
    }
    fn execution_signal(&self) -> ExecutionSignal {
        ExecutionSignal::Continue
    }
    fn set_capability_dispatcher(&self, dispatcher: Arc<dyn std::any::Any + Send + Sync>) {
        if let Ok(dispatcher) = dispatcher.downcast::<CapabilityDispatcher>() {
            self.install_dispatcher(dispatcher);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Tool;

    #[test]
    fn schema_is_exact_source_object() {
        let tool = CodeTool::new();
        assert_eq!(tool.parameters()["additionalProperties"], false);
        assert_eq!(tool.parameters()["required"], serde_json::json!(["source"]));
    }

    #[test]
    fn preview_preserves_source_verbatim() {
        let source = "\n  gait.read({path: 'a\\n'});\n";
        assert_eq!(
            CodeTool::new().preview(&serde_json::json!({"source": source})),
            Some(source.into())
        );
    }

    #[tokio::test]
    async fn rejects_non_exact_arguments() {
        let tool = CodeTool::new();
        for args in [
            serde_json::json!({}),
            serde_json::json!({"source":"x","extra":1}),
            serde_json::json!({"source":false}),
        ] {
            assert!(matches!(
                tool.execute(args, &crate::Recorder::detached()).await,
                Err(ToolError::InvalidParams(_))
            ));
        }
    }
}
