use crate::provider::{ChunkStream, Provider, ProviderError};
use crate::types::{Message, ToolDef};

pub struct OpenAIProvider {
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
}

#[async_trait::async_trait]
impl Provider for OpenAIProvider {
    async fn stream(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<ChunkStream, ProviderError> {
        todo!("implemented in Task 6")
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}
