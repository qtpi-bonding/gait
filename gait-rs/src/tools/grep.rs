use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;

pub struct GrepTool { codebase_path: Arc<PathBuf> }
impl GrepTool {
    pub fn new(codebase_path: PathBuf) -> Self { Self { codebase_path: Arc::new(codebase_path) } }
}
#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str { "grep" }
    fn description(&self) -> &str { "stub" }
    fn parameters(&self) -> &serde_json::Value { todo!() }
    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<serde_json::Value> { todo!() }
}
