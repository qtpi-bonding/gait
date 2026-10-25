use coded_error::ErrorClass;
use futures_util::Stream;
use gait_types::types::{Message, Tool, Usage};
use std::pin::Pin;
use std::sync::Arc;

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

/// Semantic purpose of a provider request. Providers may need this context
/// because a backend can accept different parameters for ordinary generation
/// and for a compaction checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Turn,
    Compaction,
}

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

/// Which phase of the request hit a timeout. Used by [`ProviderError::Timeout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPhase {
    Connect,
    Ttfb,
    Idle,
    Absolute,
    Request,
}

/// Class of a provider error. The retry decision is `ProviderError::is_retryable`
/// and is the only classification input: Transient exactly when it is true.
/// Other 4xx responses are the request's fault (Logical); the rest are ours or
/// the upstream's (Infrastructure).
fn provider_class(e: &ProviderError) -> ErrorClass {
    if e.is_retryable() {
        return ErrorClass::Transient;
    }
    match e {
        ProviderError::Http { status, .. } if (400..500).contains(status) => ErrorClass::Logical,
        _ => ErrorClass::Infrastructure,
    }
}

#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.provider")]
pub enum ProviderError {
    #[error("connect error: {0}")]
    #[coded(code = "connect", class_with = provider_class)]
    Connect(String),
    #[error("timeout ({phase:?})")]
    #[coded(code = "timeout", class_with = provider_class)]
    Timeout { phase: TimeoutPhase },
    #[error("HTTP error {status}: {body}")]
    #[coded(code = "http", class_with = provider_class)]
    Http {
        status: u16,
        body: String,
        retry_after: Option<std::time::Duration>,
        request_id: Option<String>,
    },
    #[error("server overloaded")]
    #[coded(code = "overloaded", class_with = provider_class)]
    Overloaded,
    #[error("SSE parse error: {0}")]
    #[coded(code = "sse", class_with = provider_class)]
    Sse(String),
    #[error("stream ended after {received_bytes} bytes with no terminal marker")]
    #[coded(code = "stream_incomplete", class_with = provider_class)]
    StreamIncomplete { received_bytes: usize },
    #[error("JSON response exceeded the {max_bytes}-byte limit")]
    #[coded(code = "response_too_large", class_with = provider_class)]
    ResponseTooLarge { max_bytes: usize },
    #[error("Serialization error: {0}")]
    #[coded(code = "json", class_with = provider_class)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    #[coded(code = "other", class_with = provider_class)]
    Other(String),
}

impl ProviderError {
    /// Map a raw `reqwest::Error` into a classified `ProviderError`.
    /// Raw request execution is centralized in `ProviderTransport`; keeping the
    /// mapping here preserves the provider-neutral error vocabulary.
    pub fn from_reqwest(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            ProviderError::Timeout {
                phase: TimeoutPhase::Connect,
            }
        } else if e.is_connect() {
            ProviderError::Connect(e.to_string())
        } else {
            ProviderError::Other(e.to_string())
        }
    }

    /// Whether the caller should attempt another connection. Phase-1 only —
    /// mid-stream failures (StreamIncomplete, Overloaded, Sse) are always terminal.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Connect(_) => true,
            ProviderError::Timeout { phase } => {
                matches!(phase, TimeoutPhase::Connect | TimeoutPhase::Ttfb)
            }
            ProviderError::Http { status, .. } => *status == 429 || (500..=504).contains(status),
            // Overloaded, Sse, StreamIncomplete, Json, Other — all terminal.
            _ => false,
        }
    }

    /// Server-supplied retry hint, when one was present on the response.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        if let ProviderError::Http { retry_after, .. } = self {
            *retry_after
        } else {
            None
        }
    }
}

// The core trait every LLM provider implements
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    // Stream tokens and tool call events for one LLM turn
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
        params: &gait_types::config::LlmParamsConfig,
        tail_context: Option<&str>,
    ) -> Result<ChunkStream, ProviderError>;

    /// Stream a request with semantic purpose. Existing providers retain the
    /// ordinary stream behavior by default; providers with request-specific
    /// wire constraints can override this method.
    async fn stream_with_kind(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Arc<dyn Tool>],
        params: &gait_types::config::LlmParamsConfig,
        tail_context: Option<&str>,
        _kind: RequestKind,
    ) -> Result<ChunkStream, ProviderError> {
        self.stream(system, messages, tools, params, tail_context)
            .await
    }

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

    use coded_error::CodedError;

    fn every_provider_error() -> Vec<ProviderError> {
        vec![
            ProviderError::Connect("refused".into()),
            ProviderError::Timeout {
                phase: TimeoutPhase::Connect,
            },
            ProviderError::Timeout {
                phase: TimeoutPhase::Ttfb,
            },
            ProviderError::Timeout {
                phase: TimeoutPhase::Idle,
            },
            ProviderError::Http {
                status: 429,
                body: String::new(),
                retry_after: None,
                request_id: None,
            },
            ProviderError::Http {
                status: 500,
                body: String::new(),
                retry_after: None,
                request_id: None,
            },
            ProviderError::Http {
                status: 400,
                body: String::new(),
                retry_after: None,
                request_id: None,
            },
            ProviderError::Http {
                status: 505,
                body: String::new(),
                retry_after: None,
                request_id: None,
            },
            ProviderError::Overloaded,
            ProviderError::Sse("bad frame".into()),
            ProviderError::StreamIncomplete { received_bytes: 12 },
            ProviderError::ResponseTooLarge { max_bytes: 10 },
            ProviderError::Json(serde_json::from_str::<u8>("x").unwrap_err()),
            ProviderError::Other("boom".into()),
        ]
    }

    #[test]
    fn provider_error_codes_are_unique() {
        coded_error::check_codes("gait.provider.", &[ProviderError::ALL_CODES]).unwrap();
    }

    #[test]
    fn provider_error_variant_codes_and_classes() {
        let expected = [
            ("gait.provider.connect", ErrorClass::Transient),
            ("gait.provider.timeout", ErrorClass::Transient),
            ("gait.provider.timeout", ErrorClass::Transient),
            ("gait.provider.timeout", ErrorClass::Infrastructure),
            ("gait.provider.http", ErrorClass::Transient),
            ("gait.provider.http", ErrorClass::Transient),
            ("gait.provider.http", ErrorClass::Logical),
            ("gait.provider.http", ErrorClass::Infrastructure),
            ("gait.provider.overloaded", ErrorClass::Infrastructure),
            ("gait.provider.sse", ErrorClass::Infrastructure),
            ("gait.provider.stream_incomplete", ErrorClass::Infrastructure),
            ("gait.provider.response_too_large", ErrorClass::Infrastructure),
            ("gait.provider.json", ErrorClass::Infrastructure),
            ("gait.provider.other", ErrorClass::Infrastructure),
        ];
        let errors = every_provider_error();
        assert_eq!(errors.len(), expected.len());
        for (e, (code, class)) in errors.iter().zip(expected) {
            assert_eq!(e.code(), code, "code of {e:?}");
            assert_eq!(e.class(), class, "class of {e:?}");
        }
    }

    #[test]
    fn provider_error_class_matches_is_retryable() {
        for e in every_provider_error() {
            assert_eq!(
                e.class() == ErrorClass::Transient,
                e.is_retryable(),
                "Transient must agree with is_retryable for {e:?}"
            );
        }
    }

    #[test]
    fn provider_http_4xx_is_logical() {
        for status in [400u16, 401, 403, 404, 422] {
            let e = ProviderError::Http {
                status,
                body: String::new(),
                retry_after: None,
                request_id: None,
            };
            assert_eq!(e.class(), ErrorClass::Logical, "status {status}");
        }
    }
}
