use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Mirrors Vercel AI SDK's CoreMessage union
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    User { content: MessageContent },
    Assistant { content: AssistantContent },
    Tool { content: Vec<ToolResult> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssistantContent {
    Text(String),
    Parts(Vec<AssistantPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AssistantPart {
    Text { text: String },
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    #[serde(rename = "type")]
    pub kind: ToolResultKind,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolResultKind {
    ToolResult,
}

// Tool definition — description + JSON schema + async executor
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema object
    pub execute: Box<dyn Fn(serde_json::Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<serde_json::Value>> + Send>> + Send + Sync>,
}

impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
}

impl Usage {
    pub fn add(&self, other: &Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cache_read_tokens: self.cache_read_tokens + other.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens + other.cache_write_tokens,
        }
    }
}

// Emitted per-step by stream_text, mirrors onStepFinish payload
#[derive(Debug, Clone)]
pub struct StepFinish {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    pub usage: Usage,
}

/// Whether a tool is safe to execute concurrently with other parallel-mode tools.
/// Serial tools run one at a time (default). Parallel tools can run in a concurrent batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    #[default]
    Serial,
    Parallel,
}

/// One thing a tool needs approved. `display` is shown to the user and matched
/// against the allow list. `patterns` are the full allow-list entries the user
/// can choose to save, from broadest to most specific.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub display: String,
    pub patterns: Vec<String>,
}

/// What permission gating a tool requires.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolPermission {
    /// Skip the gate entirely — reserved for `output` tool only.
    Passthrough,
    /// Gate against the allow list. One entry per "thing" being approved.
    RequiresApproval(Vec<PermissionRequest>),
}

/// What happens after a tool executes (or instead of executing).
/// `turn.rs` uses this to dispatch without hardcoding tool names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionSignal {
    /// Execute normally, push result back to model, continue the inner agentic loop.
    Continue,
    /// Calling this tool IS the exit signal — emit TurnComplete, return Done immediately.
    /// No tool result sent back to the model.
    TurnComplete,
    /// Capture items from args, emit Clarify event, return TurnResult::Clarify immediately.
    Clarify,
    /// Capture items from args, emit Inform event, return TurnResult::Inform immediately.
    Inform,
    /// Mutate the active tool list (load or unload), execute for transcript, continue loop.
    LoadContext,
}

/// Extensible tool interface. Implement this to define a callable tool for `stream_text`.
/// `ToolDef` implements this for backward-compat with ad-hoc closure-based tools.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> &serde_json::Value;
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<serde_json::Value>;

    /// Whether this tool is safe to run concurrently alongside other parallel-mode tools.
    /// Read-only tools (read, glob, grep, ls, codesearch, webfetch) return Parallel.
    /// Anything with side effects (write, edit, shell, mcp, output, clarify) returns Serial.
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Serial }

    /// What this tool needs approved before running.
    /// Default: requires approval with just the tool name as the pattern.
    fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: self.name().to_string(),
            patterns: vec![self.name().to_string()],
        }])
    }

    /// How turn.rs should dispatch this tool.
    /// Default: execute normally and continue the inner loop.
    fn execution_signal(&self) -> ExecutionSignal {
        ExecutionSignal::Continue
    }

    /// Convenience: box self into `Box<dyn Tool>`.
    fn boxed(self) -> Box<dyn Tool>
    where
        Self: Sized + 'static,
    {
        Box::new(self)
    }
}

#[async_trait::async_trait]
impl Tool for ToolDef {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }
    fn parameters(&self) -> &serde_json::Value { &self.parameters }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        // (self.execute) with parens = field access, not recursive method call.
        (self.execute)(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_message_roundtrip() {
        let msg = Message::User { content: MessageContent::Text("hello".into()) };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        if let Message::User { content: MessageContent::Text(s) } = back {
            assert_eq!(s, "hello");
        } else {
            panic!("roundtrip failed");
        }
    }

    #[test]
    fn assistant_with_tool_call_roundtrip() {
        let msg = Message::Assistant {
            content: AssistantContent::Parts(vec![
                AssistantPart::ToolCall(ToolCall {
                    tool_call_id: "id1".into(),
                    tool_name: "read".into(),
                    args: serde_json::json!({ "path": "/foo" }),
                }),
            ]),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        if let Message::Assistant { content: AssistantContent::Parts(parts) } = back {
            assert_eq!(parts.len(), 1);
        } else {
            panic!("roundtrip failed");
        }
    }

    #[test]
    fn usage_add() {
        let a = Usage { input_tokens: 10, output_tokens: 20, cache_read_tokens: 100, cache_write_tokens: 50 };
        let b = Usage { input_tokens: 5, output_tokens: 3, cache_read_tokens: 200, cache_write_tokens: 0 };
        let c = a.add(&b);
        assert_eq!(c.input_tokens, 15);
        assert_eq!(c.output_tokens, 23);
        assert_eq!(c.cache_read_tokens, 300);
        assert_eq!(c.cache_write_tokens, 50);
    }

    #[tokio::test]
    async fn tool_def_implements_tool_trait() {
        let tool = ToolDef {
            name: "greet".into(),
            description: "says hi".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("hello".into()))
            })),
        };
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(result, serde_json::Value::String("hello".into()));
        assert_eq!(tool.name(), "greet");
    }

    fn make_test_tool_def(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: format!("test tool {name}"),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::Null)
            })),
        }
    }

    #[test]
    fn tool_def_permission_default_is_tool_name() {
        let tool = make_test_tool_def("read");
        let perm = tool.permission(&serde_json::json!({}));
        match perm {
            ToolPermission::RequiresApproval(reqs) => {
                assert_eq!(reqs.len(), 1);
                assert_eq!(reqs[0].display, "read");
                assert_eq!(reqs[0].patterns, vec!["read".to_string()]);
            }
            other => panic!("expected RequiresApproval, got {:?}", other),
        }
    }

    #[test]
    fn tool_def_execution_signal_default_is_continue() {
        let tool = make_test_tool_def("read");
        assert_eq!(tool.execution_signal(), ExecutionSignal::Continue);
    }

    #[test]
    fn permission_request_fields() {
        let req = PermissionRequest { display: "foo".into(), patterns: vec!["foo".to_string()] };
        assert_eq!(req.display, "foo");
        assert_eq!(req.patterns, vec!["foo"]);
    }
}
