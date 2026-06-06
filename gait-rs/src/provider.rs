use crate::types::{Message, ToolDef, Usage};
use std::pin::Pin;
use futures_util::Stream;

// A chunk of streaming output from the LLM
#[derive(Debug)]
pub enum StreamChunk {
    Text(String),
    ToolCallDelta { tool_call_id: String, tool_name: String, args_delta: String },
    ToolCallComplete { tool_call_id: String, tool_name: String, args: serde_json::Value },
    StepUsage(Usage),
    Done,
}

pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, ProviderError>> + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP error {status}: {body}")]
    Http { status: u16, body: String },
    #[error("SSE parse error: {0}")]
    Sse(String),
    #[error("Serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("{0}")]
    Other(String),
}

// The core trait every LLM provider implements
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    // Stream tokens and tool call events for one LLM turn
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<ChunkStream, ProviderError>;

    fn model_id(&self) -> &str;
}
