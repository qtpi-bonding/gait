//! Engine-neutral contract for an in-process local language model.
//!
//! This crate deliberately has no native inference dependency.  The provider
//! adapter can be tested with [`FakeGenerator`] on any machine; native engines
//! such as `gait-llama-cpp` implement [`TextGenerator`] below it.

use async_trait::async_trait;
use futures_util::Stream;
use std::pin::Pin;

#[derive(Debug, Clone, PartialEq)]
pub struct GenerationRequest {
    pub system: String,
    pub messages: Vec<GenerationMessage>,
    pub tools: Vec<GenerationTool>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GenerationMessage {
    User {
        content: String,
    },
    Assistant {
        content: String,
        tool_calls: Vec<GenerationToolCall>,
    },
    ToolResult {
        call_id: String,
        name: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenerationTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenerationToolCall {
    pub id: String,
    pub name: String,
    /// Canonical JSON object text, never a model-specific argument syntax.
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GenerationChunk {
    Text(String),
    ToolCall(GenerationToolCall),
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },
    Done,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GenerationError {
    #[error("local model is unavailable: {0}")]
    Unavailable(String),
    #[error("local model request is invalid: {0}")]
    InvalidRequest(String),
    #[error("local model failed: {0}")]
    Engine(String),
}

pub type GenerationStream =
    Pin<Box<dyn Stream<Item = Result<GenerationChunk, GenerationError>> + Send>>;

#[async_trait]
pub trait TextGenerator: Send + Sync {
    async fn generate(
        &self,
        request: GenerationRequest,
    ) -> Result<GenerationStream, GenerationError>;
}

/// Deterministic local-model double for provider and turn-boundary tests.
pub struct FakeGenerator {
    pub chunks: Vec<Result<GenerationChunk, GenerationError>>,
}

#[async_trait]
impl TextGenerator for FakeGenerator {
    async fn generate(
        &self,
        _request: GenerationRequest,
    ) -> Result<GenerationStream, GenerationError> {
        Ok(Box::pin(futures_util::stream::iter(self.chunks.clone())))
    }
}
