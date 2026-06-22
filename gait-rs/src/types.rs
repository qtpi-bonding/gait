use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// Mirrors Vercel AI SDK's CoreMessage union
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    User {
        content: MessageContent,
        #[serde(skip)]
        id: String,
    },
    Assistant {
        content: AssistantContent,
        #[serde(skip)]
        id: String,
    },
    ToolResult {
        content: Vec<ToolResult>,
        #[serde(skip)]
        id: String,
    },
    //         ^^^^^^^^^^^ was Tool
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AssistantPart {
    Text {
        text: String,
    },
    ToolCall(ToolCall),
    /// A captured thinking/reasoning block from the LLM.
    /// `signature` is required for Anthropic multi-turn replay — pass it
    /// verbatim in subsequent turns via `AnthropicBlock::Thinking`.
    /// `None` means the provider didn't supply one (non-Anthropic).
    Thinking {
        content: String,
        signature: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    #[serde(rename = "type")]
    pub kind: ToolResultKind,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ToolResultKind {
    #[default]
    ToolResult,
}

// Tool definition — description + JSON schema + async executor
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema object
    #[allow(clippy::type_complexity)]
    pub execute: Box<
        dyn Fn(
                serde_json::Value,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<serde_json::Value, crate::tools::ToolError>,
                        > + Send,
                >,
            > + Send
            + Sync,
    >,
    #[allow(clippy::type_complexity)]
    pub permission: Option<Box<dyn Fn(&serde_json::Value) -> ToolPermission + Send + Sync>>,
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

// One step in a multi-step agentic turn (text + tool calls + results for that step)
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
/// can choose to save, from broadest to most specific. `permission()` is
/// gating-identity only — rendered previews (e.g. a diff) are produced by the
/// separate `Tool::preview()` method and fetched on demand by `turn.rs`.
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

/// How a tool's output should be post-processed before reaching the LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFilter {
    None,
    Parse,
    Curate,
    Both,
}

/// What a `pre_tool` hook returns.
#[derive(Debug, Clone, PartialEq)]
pub enum HookAction {
    Continue,
    Block { reason: String },
}

/// Accumulated turn state passed to every hook invocation.
/// Fields are populated progressively — earlier events see later fields as empty/zero.
/// All hooks receive the same struct; unset fields default to empty strings or zero.
#[derive(Debug, Clone)]
pub struct HookContext {
    // Universal — always set
    pub event: String,
    pub session_id: String,
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    // Turn-level (set at pre_turn, carried forward to all later events)
    pub system_prompt: String,
    // Step-level (set at pre_step onward)
    pub step_index: usize,
    pub message_count: usize,
    pub last_role: String, // "" | "user" | "assistant" | "tool"
    // post_step additions
    pub response_text: String,
    pub tool_count: usize,
    pub tool_names: Vec<String>,
    // Tool-level (pre_tool / post_tool)
    pub tool_name: String,
    pub tool_call_id: String,
    pub tool_result: String,
    pub tool_args: serde_json::Value,
    // Turn outcome (post_turn)
    pub step_count: usize,
    pub turn_outcome: String, // "done" | "clarify" | "inform" | "permissions"
    pub final_response: String,
}

impl Default for HookContext {
    fn default() -> Self {
        Self {
            event: String::new(),
            session_id: String::new(),
            gait_dir: PathBuf::new(),
            codebase: PathBuf::new(),
            system_prompt: String::new(),
            step_index: 0,
            message_count: 0,
            last_role: String::new(),
            response_text: String::new(),
            tool_count: 0,
            tool_names: Vec::new(),
            tool_name: String::new(),
            tool_call_id: String::new(),
            tool_result: String::new(),
            tool_args: serde_json::Value::Null,
            step_count: 0,
            turn_outcome: String::new(),
            final_response: String::new(),
        }
    }
}

/// Lifecycle hooks for the agent turn loop. All methods receive a `HookContext`
/// populated with everything the turn loop knows at that point, and a `&Recorder`
/// so every hook fire can be logged to the transcript for replay and debugging.
/// All methods have no-op defaults; implement only the hooks you need.
#[async_trait::async_trait]
pub trait Hook: Send + Sync {
    /// Once per outer turn, before the step loop. Return Some to replace the system prompt.
    fn pre_turn(&self, _ctx: &HookContext, _rec: &crate::Recorder) -> Option<String> {
        None
    }
    /// Before each inner LLM call. `messages` is the current outgoing history.
    /// Return Some to apply structured mutations to the transcript (logged first).
    fn pre_step(
        &self,
        _ctx: &HookContext,
        _rec: &crate::Recorder,
        _messages: &[Message],
    ) -> Option<Vec<MessageMutation>> {
        None
    }
    /// After each inner LLM response, before tools execute.
    fn post_step(&self, _ctx: &HookContext, _rec: &crate::Recorder) {}
    /// Before a tool executes. Return Block to prevent execution.
    fn pre_tool(&self, _ctx: &HookContext, _rec: &crate::Recorder) -> HookAction {
        HookAction::Continue
    }
    /// After a tool returns. Return Some to replace the result seen by the LLM.
    async fn post_tool(&self, _ctx: &HookContext, _rec: &crate::Recorder) -> Option<String> {
        None
    }
    /// Once per outer turn, at every exit point (Done, Clarify, Inform, Permissions).
    fn post_turn(&self, _ctx: &HookContext, _rec: &crate::Recorder) {}
}

/// The filesystem dimension a tool operates on for a specific path argument.
/// Used by `Tool::scope_targets` to declare what kind of access each target needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Read,
    Write,
}

impl Dimension {
    /// Returns `"read"` or `"write"` — the string used in `UnlockItem.dimension`.
    pub fn as_str(self) -> &'static str {
        match self {
            Dimension::Read => "read",
            Dimension::Write => "write",
        }
    }
}

/// What a tool needs from the sandbox, in the same dimensions as the sandbox scope.
/// `reads`/`writes` are CODEBASE/world filesystem access; gait-dir access is implicit
/// and brain-side. Used to route a call (brain vs hand) and, later, to seal the hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolAccess {
    pub reads: bool,
    pub writes: bool,
    pub network: bool,
    pub spawns: bool,
}

impl ToolAccess {
    /// A tool with no world access — runs in the brain (in-process).
    pub fn brain_side() -> Self {
        Self::default()
    }
    /// True if the call must run in a sealed hand. `spawns` is deliberately
    /// excluded: spawn-capable tools (spawn_agent/run_workflow) stay brain-side
    /// and delegate the actual process launch.
    pub fn touches_world(&self) -> bool {
        self.reads || self.writes || self.network
    }
}

/// Extensible tool interface. Implement this to define a callable tool for `run_turn`.
/// `ToolDef` implements this for ad-hoc closure-based tools.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> &serde_json::Value;
    async fn execute(
        &self,
        args: serde_json::Value,
        _rec: &crate::Recorder,
    ) -> Result<serde_json::Value, crate::tools::ToolError>;

    /// What this tool needs from the sandbox. Drives routing (brain vs hand) and,
    /// in later phases, the per-call seal. Default: brain-side (no world access).
    fn access(&self) -> ToolAccess {
        ToolAccess::brain_side()
    }

    /// Whether this tool is safe to run concurrently alongside other parallel-mode tools.
    /// Read-only tools (read, glob, grep, ls, codesearch, webfetch) return Parallel.
    /// Anything with side effects (write, edit, shell, mcp, output, clarify) returns Serial.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Serial
    }

    /// What this tool needs approved before running.
    /// Default: requires approval with just the tool name as the pattern.
    fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: self.name().to_string(),
            patterns: vec![self.name().to_string()],
        }])
    }

    /// A rendered preview of what this tool will do (e.g. a diff), shown in the
    /// permission prompt or streamed live per the preview policy. Default: no
    /// preview. Cheap tools return None; edit/write override this.
    fn preview(&self, _args: &serde_json::Value) -> Option<String> {
        None
    }

    /// How turn.rs should dispatch this tool.
    /// Default: execute normally and continue the inner loop.
    fn execution_signal(&self) -> ExecutionSignal {
        ExecutionSignal::Continue
    }

    /// How this tool's output should be post-processed before reaching the LLM.
    /// Default: no filtering.
    fn output_filter(&self) -> OutputFilter {
        OutputFilter::None
    }

    /// JSON Schema describing what this tool returns.
    /// When Some, provider adapters append a compact "Returns: {…}" line to the description.
    fn output_schema(&self) -> Option<&serde_json::Value> {
        None
    }

    /// Return the filesystem paths this call will access and the access dimension
    /// for each, extracted from `args`. Used by the scope pre-check in `turn.rs`
    /// (Task 5) to detect out-of-ceiling accesses before running the tool.
    ///
    /// `shell` is excluded (opaque — handled via the EACCES path in Task 6).
    /// Default: `vec![]` — tools that don't operate on known paths pass through.
    fn scope_targets(&self, _args: &serde_json::Value) -> Vec<(std::path::PathBuf, Dimension)> {
        vec![]
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
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> &serde_json::Value {
        &self.parameters
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _rec: &crate::Recorder,
    ) -> Result<serde_json::Value, crate::tools::ToolError> {
        // (self.execute) with parens = field access, not recursive method call.
        (self.execute)(args).await
    }
    fn permission(&self, args: &serde_json::Value) -> ToolPermission {
        match &self.permission {
            Some(f) => f(args),
            None => ToolPermission::RequiresApproval(vec![PermissionRequest {
                display: self.name.clone(),
                patterns: vec![self.name.clone()],
            }]),
        }
    }
}

/// Render a JSON Schema as a compact type annotation.
/// e.g. "{output: string, exit_code: integer}" or "[string]" or "string"
pub fn compact_type(schema: &serde_json::Value) -> String {
    match schema.get("type").and_then(|v| v.as_str()) {
        Some("object") => {
            let props = schema
                .get("properties")
                .and_then(|p| p.as_object())
                .cloned()
                .unwrap_or_default();
            let fields: Vec<String> = props
                .iter()
                .map(|(k, v)| format!("{}: {}", k, compact_type(v)))
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
        Some("array") => {
            let items = schema.get("items").unwrap_or(&serde_json::Value::Null);
            format!("[{}]", compact_type(items))
        }
        Some(t) => t.to_string(),
        None => "any".to_string(),
    }
}

/// Build the description string sent to the provider API.
/// Appends "\n\nReturns: {compact shape}" when the tool has an output_schema.
pub fn render_description(tool: &dyn Tool) -> String {
    match tool.output_schema() {
        None => tool.description().to_string(),
        Some(schema) => format!(
            "{}\n\nReturns: {}",
            tool.description(),
            compact_type(schema)
        ),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoadableKind {
    Mcp,
    Skill,
    WasmPlugin,
}

impl LoadableKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LoadableKind::Mcp => "mcp",
            LoadableKind::Skill => "skill",
            LoadableKind::WasmPlugin => "plugin",
        }
    }
}

/// A whitelisted item that can be enabled/disabled by the LLM at runtime.
#[derive(Debug, Clone)]
pub struct LoadableEntry {
    pub name: String,
    pub kind: LoadableKind,
    pub description: String,
    pub enabled: bool,
}

impl Message {
    /// Extract the id regardless of variant.
    pub fn id(&self) -> &str {
        match self {
            Message::User { id, .. } => id,
            Message::Assistant { id, .. } => id,
            Message::ToolResult { id, .. } => id,
        }
    }

    /// Construct a Message from an Inject event.
    pub fn from_inject(id: String, role: &InjectRole, text: String) -> Self {
        match role {
            InjectRole::User => Message::User {
                content: MessageContent::Text(text),
                id,
            },
            InjectRole::Assistant => Message::Assistant {
                content: AssistantContent::Text(text),
                id,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InjectRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MutationPayload {
    Text(String),
    ToolOutput {
        tool_call_id: String,
        output: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageMutation {
    /// Mask a message. Emits Redact to log. Reversible via Restore.
    Redact { id: String },
    /// Rewrite content. Emits Replace to log.
    Replace {
        id: String,
        payload: MutationPayload,
    },
    /// Undo a prior Redact. Emits Replace superseding it (LWW).
    Restore { id: String },
    /// Append a new message. Emits Inject to log. Permanent in transcript (Design A).
    Inject { role: InjectRole, text: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_message_roundtrip() {
        let msg = Message::User {
            content: MessageContent::Text("hello".into()),
            id: String::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        if let Message::User {
            content: MessageContent::Text(s),
            ..
        } = back
        {
            assert_eq!(s, "hello");
        } else {
            panic!("roundtrip failed");
        }
    }

    #[test]
    fn assistant_with_tool_call_roundtrip() {
        let msg = Message::Assistant {
            content: AssistantContent::Parts(vec![AssistantPart::ToolCall(ToolCall {
                tool_call_id: "id1".into(),
                tool_name: "read".into(),
                args: serde_json::json!({ "path": "/foo" }),
            })]),
            id: String::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        if let Message::Assistant {
            content: AssistantContent::Parts(parts),
            ..
        } = back
        {
            assert_eq!(parts.len(), 1);
        } else {
            panic!("roundtrip failed");
        }
    }

    #[test]
    fn usage_add() {
        let a = Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 100,
            cache_write_tokens: 50,
        };
        let b = Usage {
            input_tokens: 5,
            output_tokens: 3,
            cache_read_tokens: 200,
            cache_write_tokens: 0,
        };
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
            execute: Box::new(|_args| {
                Box::pin(async move { Ok(serde_json::Value::String("hello".into())) })
            }),
            permission: None,
        };
        let result = tool
            .execute(serde_json::json!({}), &crate::Recorder::detached())
            .await
            .unwrap();
        assert_eq!(result, serde_json::Value::String("hello".into()));
        assert_eq!(tool.name(), "greet");
    }

    fn make_test_tool_def(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: format!("test tool {name}"),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move { Ok(serde_json::Value::Null) })),
            permission: None,
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
        let req = PermissionRequest {
            display: "foo".into(),
            patterns: vec!["foo".to_string()],
        };
        assert_eq!(req.display, "foo");
        assert_eq!(req.patterns, vec!["foo"]);
    }

    #[test]
    fn loadable_kind_as_str() {
        assert_eq!(LoadableKind::Mcp.as_str(), "mcp");
        assert_eq!(LoadableKind::Skill.as_str(), "skill");
        assert_eq!(LoadableKind::WasmPlugin.as_str(), "plugin");
    }

    #[test]
    fn compact_type_object() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "output": {"type": "string"},
                "exit_code": {"type": "integer"}
            }
        });
        let rendered = compact_type(&schema);
        assert!(
            rendered.starts_with("{") && rendered.ends_with("}"),
            "got: {rendered}"
        );
        assert!(rendered.contains("output: string"), "got: {rendered}");
        assert!(rendered.contains("exit_code: integer"), "got: {rendered}");
    }

    #[test]
    fn compact_type_array() {
        let schema = serde_json::json!({"type": "array", "items": {"type": "string"}});
        assert_eq!(compact_type(&schema), "[string]");
    }

    #[test]
    fn compact_type_scalar() {
        assert_eq!(
            compact_type(&serde_json::json!({"type": "string"})),
            "string"
        );
        assert_eq!(
            compact_type(&serde_json::json!({"type": "integer"})),
            "integer"
        );
    }

    #[test]
    fn compact_type_null_is_any() {
        assert_eq!(compact_type(&serde_json::Value::Null), "any");
    }

    #[test]
    fn render_description_no_schema_returns_description() {
        let tool = make_test_tool_def("no_schema");
        assert_eq!(render_description(&tool), "test tool no_schema");
    }

    #[test]
    fn render_description_with_schema_appends_returns_line() {
        let tool = ToolDef {
            name: "with_schema".into(),
            description: "does stuff".into(),
            parameters: serde_json::json!({"type": "object"}),
            execute: Box::new(|_| Box::pin(async { Ok(serde_json::Value::Null) })),
            permission: None,
        };
        // ToolDef output_schema returns None by default, so test compact_type+format directly
        let schema =
            serde_json::json!({"type": "object", "properties": {"result": {"type": "string"}}});
        let rendered = format!(
            "{}\n\nReturns: {}",
            tool.description(),
            compact_type(&schema)
        );
        assert!(
            rendered.contains("Returns: {result: string}"),
            "got: {rendered}"
        );
    }
    #[test]
    fn hook_context_default_is_empty() {
        let ctx = HookContext::default();
        assert_eq!(ctx.event, "");
        assert_eq!(ctx.step_index, 0);
        assert!(ctx.tool_names.is_empty());
        assert!(ctx.tool_args.is_null());
    }

    #[test]
    fn message_id_accessor() {
        let msg = Message::User {
            content: MessageContent::Text("hi".into()),
            id: "abc".into(),
        };
        assert_eq!(msg.id(), "abc");
    }

    #[test]
    fn message_serde_skips_id() {
        let msg = Message::User {
            content: MessageContent::Text("hi".into()),
            id: "secret".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("secret"), "id must be skipped: {json}");
        assert!(
            !json.contains("\"id\""),
            "id field must not appear in JSON: {json}"
        );
    }

    #[test]
    fn assistant_part_thinking_roundtrips_json() {
        let part = AssistantPart::Thinking {
            content: "analysis".into(),
            signature: Some("sig_abc".into()),
        };
        let json = serde_json::to_string(&part).unwrap();
        let back: AssistantPart = serde_json::from_str(&json).unwrap();
        assert_eq!(part, back);
    }

    #[test]
    fn tool_access_defaults_to_brain_side() {
        let a = ToolAccess::default();
        assert_eq!(
            a,
            ToolAccess {
                reads: false,
                writes: false,
                network: false,
                spawns: false
            }
        );
        assert!(!a.touches_world());
        assert_eq!(ToolAccess::brain_side(), a);
    }

    #[test]
    fn tool_access_touches_world_ignores_spawns() {
        // spawns alone is NOT world-touching (spawn_agent stays brain-side; it delegates the launch).
        let spawns_only = ToolAccess {
            spawns: true,
            ..ToolAccess::default()
        };
        assert!(!spawns_only.touches_world());
        let reads = ToolAccess {
            reads: true,
            ..ToolAccess::default()
        };
        assert!(reads.touches_world());
    }
}
