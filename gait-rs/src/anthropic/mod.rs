pub mod sse;
pub mod types;

use crate::provider::{ChunkStream, Provider, ProviderError};
use crate::types::{Message, Tool};
use crate::anthropic::sse::SseClient;
use crate::anthropic::types::{
    AnthropicRequest, AnthropicSystem, AnthropicSystemBlock, AnthropicTool,
    CacheControl, to_anthropic_messages,
};

pub struct AnthropicProvider {
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
    pub cache: bool,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self { api_key, model, base_url: None, cache: false }
    }
    pub fn with_base_url(api_key: String, model: String, base_url: String) -> Self {
        Self { api_key, model, base_url: Some(base_url), cache: false }
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
        let system_field = if system.is_empty() {
            None
        } else if self.cache {
            Some(AnthropicSystem::Blocks(vec![AnthropicSystemBlock::text_cached(system)]))
        } else {
            Some(AnthropicSystem::Text(system.into()))
        };

        let mut oai_tools: Vec<AnthropicTool> = tools.iter().map(|t| AnthropicTool {
            name: t.name().to_string(),
            description: t.description().to_string(),
            input_schema: t.parameters().clone(),
            cache_control: None,
        }).collect();

        // Mark last tool with cache_control so the system+tools prefix is cached as one block.
        if self.cache && !oai_tools.is_empty() {
            let last = oai_tools.last_mut().unwrap();
            last.cache_control = Some(CacheControl::ephemeral());
        }

        let req = AnthropicRequest {
            model: self.model.clone(),
            max_tokens: 8096,
            system: system_field,
            messages: to_anthropic_messages(messages),
            tools: oai_tools,
            stream: true,
        };
        let body = serde_json::to_value(&req)?;
        let client = match &self.base_url {
            Some(url) => SseClient::with_base_url(self.api_key.clone(), url.clone()),
            None => SseClient::new(self.api_key.clone()),
        };
        let stream = client.stream_request(body);
        Ok(Box::pin(stream))
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}
