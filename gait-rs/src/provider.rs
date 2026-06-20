use crate::types::{Message, Tool, Usage};
use futures_util::Stream;
use std::pin::Pin;

// A chunk of streaming output from the LLM
#[derive(Debug, Clone)]
pub enum StreamChunk {
    Text(String),
    ThinkingDelta(String), // thinking text fragment; ALWAYS emitted (no show_thinking gate)
    ThinkingSignature {
        index: u32,
        signature: String,
    },
    ToolCallDelta {
        tool_call_id: String,
        tool_name: String,
        args_delta: String,
    },
    ToolCallComplete {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    StepUsage(Usage),
    Done,
}

pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, ProviderError>> + Send>>;

/// A tool call's arguments are *always* a JSON object in both the Anthropic and
/// OpenAI wire formats. Coerce anything else (string, number, null, array) to an
/// empty object so it can never poison message history. See [`parse_tool_args`].
pub fn coerce_tool_input(v: serde_json::Value) -> serde_json::Value {
    if v.is_object() {
        v
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    }
}

/// Parse a streamed tool-call argument buffer into a *guaranteed* JSON object.
///
/// Models under load (notably MiniMax-M3) occasionally emit a truncated or
/// otherwise invalid argument buffer, e.g. `{"edits": ` instead of a complete
/// object. Storing that as a bare `Value::String` and persisting it into message
/// history poisons every subsequent provider request: the next call 400s with
/// `tool_use.input: Input should be a valid dictionary` and the session dies with
/// no recovery. Guaranteeing an object at the source keeps history provider-valid
/// — the tool rejects the empty args with a normal, recoverable "missing field"
/// error and the model simply retries the turn. One bad turn, not a dead session.
pub fn parse_tool_args(buf: &str) -> serde_json::Value {
    coerce_tool_input(serde_json::from_str(buf).unwrap_or(serde_json::Value::Null))
}

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
        tools: &[Box<dyn Tool>],
        params: &crate::config::LlmParamsConfig,
        tail_context: Option<&str>,
    ) -> Result<ChunkStream, ProviderError>;

    fn model_id(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_tool_args_keeps_valid_object() {
        assert_eq!(
            parse_tool_args(r#"{"edits":[1,2]}"#),
            json!({"edits":[1,2]})
        );
    }

    #[test]
    fn parse_tool_args_empty_object_for_truncated_buffer() {
        // The real-world poison: a truncated input_json buffer from the model.
        assert_eq!(parse_tool_args(r#"{"edits": "#), json!({}));
    }

    #[test]
    fn parse_tool_args_empty_object_for_bare_string() {
        // Valid JSON, but a string — not an object. Must not slip through.
        assert_eq!(parse_tool_args(r#""hello""#), json!({}));
    }

    #[test]
    fn parse_tool_args_empty_object_for_empty_and_nonobject() {
        assert_eq!(parse_tool_args(""), json!({}));
        assert_eq!(parse_tool_args("42"), json!({}));
        assert_eq!(parse_tool_args("[1,2,3]"), json!({}));
    }

    #[test]
    fn coerce_tool_input_passes_objects_and_floors_the_rest() {
        assert_eq!(coerce_tool_input(json!({"a":1})), json!({"a":1}));
        assert_eq!(coerce_tool_input(json!("{\"edits\": ")), json!({}));
        assert_eq!(coerce_tool_input(json!(null)), json!({}));
        assert_eq!(coerce_tool_input(json!([1, 2])), json!({}));
    }

    #[test]
    fn thinking_delta_variant_exists() {
        // Purely a compile-time check that StreamChunk::ThinkingDelta is defined
        let chunk = StreamChunk::ThinkingDelta("hello thinking".into());
        match chunk {
            StreamChunk::ThinkingDelta(s) => assert_eq!(s, "hello thinking"),
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn thinking_signature_variant_exists() {
        // Purely a compile-time check that StreamChunk::ThinkingSignature is defined
        let chunk = StreamChunk::ThinkingSignature {
            index: 7,
            signature: "sig_xyz".into(),
        };
        match chunk {
            StreamChunk::ThinkingSignature { index, signature } => {
                assert_eq!(index, 7);
                assert_eq!(signature, "sig_xyz");
            }
            _ => panic!("unexpected variant"),
        }
    }
}
