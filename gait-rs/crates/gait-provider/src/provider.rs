use coded_error::ErrorClass;
use futures_util::Stream;
use gait_types::types::{Message, Tool, Usage};
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;

/// The most of a provider's error message that becomes part of an error's text.
const ERROR_MESSAGE_MAX_BYTES: usize = 512;
/// The most of a provider's error type, code or parameter name that does.
const ERROR_LABEL_MAX_BYTES: usize = 128;

/// `text`, cut to at most ERROR_MESSAGE_MAX_BYTES; a cut text ends with `…`.
pub(crate) fn short_message(text: &str) -> String {
    short_text(text, ERROR_MESSAGE_MAX_BYTES)
}

/// The same, with ERROR_LABEL_MAX_BYTES.
pub(crate) fn short_label(text: &str) -> String {
    short_text(text, ERROR_LABEL_MAX_BYTES)
}

fn short_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}", &text[..end])
}

/// `type=K code=C: message`, each part capped, absent parts left out.
/// None when all three are absent.
pub(crate) fn error_summary(
    kind: Option<&str>,
    code: Option<&str>,
    message: Option<&str>,
) -> Option<String> {
    let mut parts = String::new();
    if let Some(kind) = kind {
        parts.push_str(&format!("type={}", short_label(kind)));
    }
    if let Some(code) = code {
        if !parts.is_empty() {
            parts.push(' ');
        }
        parts.push_str(&format!("code={}", short_label(code)));
    }
    if let Some(message) = message {
        if !parts.is_empty() {
            parts.push_str(": ");
        }
        parts.push_str(&short_message(message));
    }
    (!parts.is_empty()).then_some(parts)
}

/// A provider's error response body. Its Display and Debug show only a short
/// summary, because the body can quote the request and this text reaches logs
/// and telemetry. `raw` is for reading fields out of it.
#[derive(Clone)]
pub struct ErrorBody {
    raw: String,
    summary: String,
}

impl ErrorBody {
    fn new(raw: String) -> Self {
        let summary = summary_for(&raw);
        Self { raw, summary }
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Replaces the summary with `message`, cut to at most 1024 bytes the
    /// same way as the built-in summary.
    pub fn with_message(self, message: &str) -> Self {
        Self {
            summary: short_text(message, 1024),
            ..self
        }
    }
}

fn summary_for(raw: &str) -> String {
    let value: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let error = value.get("error").unwrap_or(&Value::Null);
    let (kind, code, message) = if error.is_string() {
        (None, None, error.as_str())
    } else {
        let source = if error.is_object() { error } else { &value };
        let kind = source.get("type").and_then(Value::as_str);
        let code = source
            .get("code")
            .map(|code| {
                code.as_str()
                    .map(str::to_owned)
                    .or_else(|| code.as_u64().map(|n| n.to_string()))
                    .unwrap_or_default()
            })
            .filter(|code| !code.is_empty());
        let message = source
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| value.get("detail").and_then(Value::as_str))
            .or_else(|| value.get("message").and_then(Value::as_str));
        (kind, code, message)
    };
    error_summary(kind, code.as_deref(), message)
        .unwrap_or_else(|| format!("no error message in a {}-byte body", raw.len()))
}

impl From<String> for ErrorBody {
    fn from(raw: String) -> Self {
        Self::new(raw)
    }
}

impl From<&str> for ErrorBody {
    fn from(raw: &str) -> Self {
        Self::new(raw.to_owned())
    }
}

impl std::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary)
    }
}

impl std::fmt::Debug for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary)
    }
}

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
        body: ErrorBody,
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
    Json(serde_json::Error),
    #[error("{0}")]
    #[coded(code = "other", class_with = provider_class)]
    Other(String),
    /// The provider rejected the credential as expired; carries the auth
    /// error's own code and class.
    #[error(transparent)]
    #[coded(transparent)]
    Auth(#[from] gait_auth::AuthError),
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
        } else if e.is_request() && !e.is_body() && !e.is_decode() && is_dropped_connection(&e) {
            // The request failed to send and no response bytes were received,
            // so a dropped connection is as retryable as a failed connect.
            ProviderError::Connect(e.to_string())
        } else {
            ProviderError::Other(e.to_string())
        }
    }

    /// Whether the caller should attempt another connection. Phase-1 only —
    /// mid-stream failures (StreamIncomplete, Overloaded, Sse) are always terminal.
    /// This includes reqwest send failures where the peer dropped the connection
    /// before any response bytes arrived (classified as `Connect` by
    /// [`ProviderError::from_reqwest`]). The server may already have received
    /// and started processing such a request; what is certain is that no
    /// response was received, so the caller cannot have consumed any output.
    /// Replaying the identical POST is therefore safe — for a
    /// chat/completions request it is at worst a duplicated generation, with
    /// no side effect the client observed.
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

/// A serde error's own text quotes the value it rejected, which can be reply
/// text, so only its kind and place are kept.
impl From<serde_json::Error> for ProviderError {
    fn from(error: serde_json::Error) -> Self {
        ProviderError::Json(<serde_json::Error as serde::de::Error>::custom(format!(
            "{:?} error at line {} column {}",
            error.classify(),
            error.line(),
            error.column()
        )))
    }
}

/// Whether the reqwest error chain shows the peer dropped the connection
/// before any response bytes arrived. Phase 1 only: the request was sent
/// without any response bytes having been received. The server may have
/// received and started the request, but since no response came back the
/// caller cannot have consumed any output; replaying the identical POST is
/// safe — at worst a duplicated generation with no side effect the client
/// observed.
fn is_dropped_connection(e: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(e);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ) {
                return true;
            }
        }
        if err
            .to_string()
            .contains("connection closed before message completed")
        {
            return true;
        }
        source = err.source();
    }
    false
}

// The core trait every LLM provider implements
#[async_trait::async_trait]
// The must_use comes from the async_trait expansion (Pin<Box<dyn Future>> return).
#[allow(clippy::double_must_use)]
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
                body: String::new().into(),
                retry_after: None,
                request_id: None,
            },
            ProviderError::Http {
                status: 500,
                body: String::new().into(),
                retry_after: None,
                request_id: None,
            },
            ProviderError::Http {
                status: 400,
                body: String::new().into(),
                retry_after: None,
                request_id: None,
            },
            ProviderError::Http {
                status: 505,
                body: String::new().into(),
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
            (
                "gait.provider.stream_incomplete",
                ErrorClass::Infrastructure,
            ),
            (
                "gait.provider.response_too_large",
                ErrorClass::Infrastructure,
            ),
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

    fn http_error(status: u16, body: &str) -> ProviderError {
        ProviderError::Http {
            status,
            body: body.into(),
            retry_after: None,
            request_id: None,
        }
    }

    /// The two ways an error becomes text.
    fn texts(error: &ProviderError) -> [String; 2] {
        [error.to_string(), format!("{error:?}")]
    }

    #[test]
    fn an_http_errors_text_names_type_code_and_message_not_the_body() {
        let error = http_error(
            400,
            r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"too long"},"request":"hunter2-prompt"}"#,
        );
        for text in texts(&error) {
            for wanted in [
                "400",
                "invalid_request_error",
                "context_length_exceeded",
                "too long",
            ] {
                assert!(text.contains(wanted), "{wanted} missing: {text}");
            }
            assert!(!text.contains("hunter2-prompt"), "{text}");
        }
    }

    #[test]
    fn the_other_common_error_shapes_keep_their_message() {
        for (body, wanted) in [
            (
                r#"{"detail":"Unsupported model","echo":"hunter2"}"#,
                "Unsupported model",
            ),
            (
                r#"{"error":"invalid_token","echo":"hunter2"}"#,
                "invalid_token",
            ),
            (r#"{"message":"slow down","echo":"hunter2"}"#, "slow down"),
            (r#"{"error":{"code":4290,"echo":"hunter2"}}"#, "4290"),
        ] {
            for text in texts(&http_error(400, body)) {
                assert!(text.contains(wanted), "{wanted} missing: {text}");
                assert!(!text.contains("hunter2"), "{text}");
            }
        }
    }

    #[test]
    fn a_body_that_names_no_error_is_not_shown() {
        for body in [
            "<html>hunter2-prompt</html>",
            r#"{"echo":"hunter2-prompt"}"#,
            r#""hunter2-prompt""#,
        ] {
            for text in texts(&http_error(502, body)) {
                assert!(text.contains("502"), "{text}");
                assert!(!text.contains("hunter2-prompt"), "{text}");
            }
        }
    }

    #[test]
    fn an_http_errors_message_type_and_code_are_capped() {
        for filler in ["x", "é"] {
            let long = format!("{}hunter2-tail", filler.repeat(5000));
            let body = serde_json::json!({"error":{"type": long, "code": long, "message": long}});
            for text in texts(&http_error(400, &body.to_string())) {
                assert!(!text.contains("hunter2-tail"), "{text}");
                assert!(text.len() < 1000, "{} bytes", text.len());
            }
        }
    }

    #[test]
    fn a_json_errors_text_quotes_none_of_the_input() {
        let error =
            ProviderError::from(serde_json::from_str::<u8>(r#""hunter2-reply""#).unwrap_err());
        for text in texts(&error) {
            assert!(!text.contains("hunter2-reply"), "{text}");
        }
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            assert!(!cause.to_string().contains("hunter2-reply"), "{cause}");
            source = cause.source();
        }
    }

    #[test]
    fn provider_http_4xx_is_logical() {
        for status in [400u16, 401, 403, 404, 422] {
            let e = ProviderError::Http {
                status,
                body: String::new().into(),
                retry_after: None,
                request_id: None,
            };
            assert_eq!(e.class(), ErrorClass::Logical, "status {status}");
        }
    }

    #[test]
    fn auth_error_variant_keeps_the_expired_code() {
        let e = ProviderError::Auth(gait_auth::AuthError::Expired(
            "run `codex login` again".into(),
        ));
        assert_eq!(e.code(), "gait.auth.expired");
        assert_eq!(e.class(), coded_error::ErrorClass::Logical);
        assert_eq!(e.to_string(), "run `codex login` again");
        assert!(!e.is_retryable());
    }
}
