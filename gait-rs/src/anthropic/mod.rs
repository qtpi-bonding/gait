pub mod sse;
pub mod types;

use crate::provider::{ChunkStream, Provider, ProviderError};
use crate::types::{Message, Tool};
use crate::anthropic::sse::SseClient;
use crate::anthropic::types::{AnthropicRequest, AnthropicTool, to_anthropic_messages};

pub struct AnthropicProvider {
    pub api_key: String,
    pub model: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self { api_key, model }
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Box<dyn Tool>],
    ) -> Result<ChunkStream, ProviderError> {
        let req = AnthropicRequest {
            model: self.model.clone(),
            max_tokens: 8096,
            system: if system.is_empty() { None } else { Some(system.into()) },
            messages: to_anthropic_messages(messages),
            tools: tools.iter().map(|t| AnthropicTool {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.parameters().clone(),
            }).collect(),
            stream: true,
        };
        let body = serde_json::to_value(&req)?;
        // Build a fresh SseClient (owned) so the stream it returns is 'static.
        let client = SseClient::new(self.api_key.clone());
        let stream = client.stream_request(body);
        Ok(Box::pin(stream))
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}
