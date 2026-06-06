pub mod sse;
pub mod types;

use crate::provider::{ChunkStream, Provider, ProviderError};
use crate::types::{Message, ToolDef};

pub struct AnthropicProvider {
    pub api_key: String,
    pub model: String,
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    async fn stream(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<ChunkStream, ProviderError> {
        todo!("implemented in Task 5")
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}
