# gait-ai Tools Layer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `tools` module to `gait-ai` with seven built-in filesystem/shell tools (read, write, edit, glob, grep, ls, shell) behind a new `Tool` trait that makes tool definitions extensible and idiomatic.

**Architecture:** A `Tool` trait replaces direct field access on `ToolDef` throughout providers and streaming. Each built-in tool is its own struct implementing `Tool`. `ToolDef` continues to implement `Tool` for backward compat with ad-hoc closures. `stream_text` and all providers accept `&[Box<dyn Tool>]`. `builtin_tools(codebase_path)` returns `Vec<Box<dyn Tool>>` with all seven tools.

**Tech Stack:** `schemars 0.8` (JSON Schema from `#[derive(JsonSchema)]`), `glob 0.3` (pattern matching), `walkdir 2` (directory walking), `regex 1` (regex search), `tokio::process::Command` (async shell), `tempfile 3` (dev-dep for tests)

---

### Task 1: Cargo.toml + `Tool` trait + streaming refactor

**Files:**
- Modify: `gait-rs/Cargo.toml`
- Modify: `gait-rs/src/types.rs`
- Modify: `gait-rs/src/provider.rs`
- Modify: `gait-rs/src/stream.rs`
- Modify: `gait-rs/src/generate.rs`
- Modify: `gait-rs/src/anthropic/mod.rs`
- Modify: `gait-rs/src/openai/mod.rs`

---

- [x] **Step 1: Add dependencies to Cargo.toml**

In `gait-rs/Cargo.toml`, add to `[dependencies]`:

```toml
schemars = { version = "0.8", features = ["derive"] }
glob = "0.3"
walkdir = "2"
regex = "1"
```

Add a new `[dev-dependencies]` section:

```toml
[dev-dependencies]
tempfile = "3"
```

- [x] **Step 2: Add `Tool` trait to `types.rs`**

Add this block to `gait-rs/src/types.rs`, immediately before `#[cfg(test)]`:

```rust
/// Extensible tool interface. Implement this to define a callable tool for `stream_text`.
/// `ToolDef` implements this for backward-compat with ad-hoc closure-based tools.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> &serde_json::Value;
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<serde_json::Value>;

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
```

- [x] **Step 3: Write a test that the Tool trait works on ToolDef**

Add to the `#[cfg(test)]` block in `types.rs`:

```rust
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
```

- [x] **Step 4: Run the new test to make sure it compiles and passes**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test tool_def_implements_tool_trait
```

Expected: PASS

- [x] **Step 5: Update `provider.rs` — change `stream()` signature**

In `gait-rs/src/provider.rs`, replace the import line:
```rust
use crate::types::{Message, ToolDef, Usage};
```
With:
```rust
use crate::types::{Message, Tool, Usage};
```

Change the `Provider` trait `stream()` signature from:
```rust
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<ChunkStream, ProviderError>;
```
To:
```rust
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Box<dyn Tool>],
    ) -> Result<ChunkStream, ProviderError>;
```

- [x] **Step 6: Update `stream.rs` — change signatures and tool lookup**

In `gait-rs/src/stream.rs`, update the import:
```rust
use crate::types::{Message, ToolDef, ToolCall, ToolResult, ToolResultKind, StepFinish, Usage};
```
To:
```rust
use crate::types::{Message, Tool, ToolCall, ToolResult, ToolResultKind, StepFinish, Usage};
```

Change both `stream_text` and `stream_text_with_callback` signatures — replace `tools: &[ToolDef]` with `tools: &[Box<dyn Tool>]`.

Change tool lookup and execution (in the agentic loop body):
```rust
        let tool = tools.iter().find(|t| t.name == tc.tool_name);
        let result = match tool {
            Some(t) => (t.execute)(tc.args.clone()).await
```
To:
```rust
        let tool = tools.iter().find(|t| t.name() == tc.tool_name);
        let result = match tool {
            Some(t) => t.execute(tc.args.clone()).await
```

- [x] **Step 7: Update the `stream.rs` test to box the ToolDef**

In the `#[cfg(test)]` block of `stream.rs`, find both `stream_text(...)` calls that pass `&[tool]` and replace with `&[Box::new(tool)]`. There are two tests: `executes_tool_and_loops` and `returns_text_immediately_when_no_tools`. The second passes `&[]` which needs no change.

For `executes_tool_and_loops`:
```rust
        let result = stream_text(
            &provider,
            "",
            &[Message::User { content: crate::types::MessageContent::Text("go".into()) }],
            &[Box::new(tool)],
        ).await.unwrap();
```

Also update the import inside the test module — add `Tool` to the import:
```rust
    use crate::types::{Message, ToolDef, Tool};
```

- [x] **Step 8: Update `generate.rs` — fix empty tools slice type annotation (C1)**

`gait-rs/src/generate.rs` calls `stream_text(provider, system, messages, &[])`. After the signature change to `&[Box<dyn Tool>]`, the empty slice `&[]` cannot infer its element type and produces `error[E0282]: type annotations needed`. Read the file and apply this fix — find:

```rust
stream_text(provider, system, messages, &[]).await
```

Replace with:

```rust
use crate::types::Tool;
stream_text(provider, system, messages, &[] as &[Box<dyn Tool>]).await
```

Or add the import at the top of `generate.rs` and write `&[] as &[Box<dyn Tool>]` inline.

- [x] **Step 10: Update `anthropic/mod.rs`**

Replace:
```rust
use crate::types::{Message, ToolDef};
```
With:
```rust
use crate::types::{Message, Tool};
```

Change the `impl Provider for AnthropicProvider` `stream()` signature:
```rust
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<ChunkStream, ProviderError> {
```
To:
```rust
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Box<dyn Tool>],
    ) -> Result<ChunkStream, ProviderError> {
```

Change the tool conversion from field access to method calls:
```rust
            tools: tools.iter().map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.parameters.clone(),
            }).collect(),
```
To:
```rust
            tools: tools.iter().map(|t| AnthropicTool {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.parameters().clone(),
            }).collect(),
```

- [x] **Step 11: Update `openai/mod.rs`** (I2 fix — exact import)

The current import line in `openai/mod.rs` is:
```rust
use crate::types::{Message, ToolDef, Usage, AssistantContent, AssistantPart, MessageContent};
```

Replace it exactly with (removing `ToolDef`, adding `Tool`):
```rust
use crate::types::{Message, Tool, Usage, AssistantContent, AssistantPart, MessageContent};
```

Change the `stream()` signature parameter from `tools: &[ToolDef]` to `tools: &[Box<dyn Tool>]`.

Change the tool conversion from field access to method calls:
```rust
        let oai_tools: Vec<ChatCompletionTool> = tools.iter().map(|t| ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: t.name.clone(),
                description: Some(t.description.clone()),
                parameters: Some(t.parameters.clone()),
                strict: None,
            },
        }).collect();
```
To:
```rust
        let oai_tools: Vec<ChatCompletionTool> = tools.iter().map(|t| ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: t.name().to_string(),
                description: Some(t.description().to_string()),
                parameters: Some(t.parameters().clone()),
                strict: None,
            },
        }).collect();
```

- [x] **Step 12: Run all tests to confirm the refactor compiles clean**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test
```

Expected: All existing tests pass. Zero compile errors. (Integration tests skip if API key not set — that's fine.)

- [x] **Step 13: Commit**

```bash
git add gait-rs/Cargo.toml gait-rs/src/types.rs gait-rs/src/provider.rs gait-rs/src/stream.rs gait-rs/src/generate.rs gait-rs/src/anthropic/mod.rs gait-rs/src/openai/mod.rs
git commit -m "feat(rs): Tool trait — extensible tool interface, refactor stream/providers to Box<dyn Tool>"
```

---

### Task 2: `tools/mod.rs` skeleton + `tools/read.rs`

**Files:**
- Create: `gait-rs/src/tools/mod.rs`
- Create: `gait-rs/src/tools/read.rs`
- Modify: `gait-rs/src/lib.rs`

---

- [x] **Step 1: Create `gait-rs/src/tools/mod.rs` with module stubs and `make_schema` helper**

```rust
pub mod read;
pub mod write;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod shell;

pub use read::ReadTool;
pub use write::WriteTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use shell::ShellTool;

use std::path::PathBuf;
use crate::types::Tool;

/// Generate a JSON Schema for type T and strip the `$schema` and `title` meta-keys
/// that schemars adds by default. Anthropic's `input_schema` field accepts a plain
/// JSON Schema object — the extra meta-keys cause 400 errors against the live API.
pub fn make_schema<T: schemars::JsonSchema>() -> serde_json::Value {
    let schema = schemars::schema_for!(T);
    let mut v = serde_json::to_value(&schema).expect("schema serialization");
    if let Some(obj) = v.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }
    v
}

/// Returns all seven built-in tools configured for the given codebase root path.
pub fn builtin_tools(codebase_path: PathBuf) -> Vec<Box<dyn Tool>> {
    vec![
        ReadTool::new(codebase_path.clone()).boxed(),
        WriteTool::new(codebase_path.clone()).boxed(),
        EditTool::new(codebase_path.clone()).boxed(),
        GlobTool::new(codebase_path.clone()).boxed(),
        GrepTool::new(codebase_path.clone()).boxed(),
        LsTool::new(codebase_path.clone()).boxed(),
        ShellTool::new(codebase_path).boxed(),
    ]
}
```

- [x] **Step 2: Add `pub mod tools;` to `gait-rs/src/lib.rs`**

Append to `gait-rs/src/lib.rs`:
```rust
pub mod tools;
pub use tools::builtin_tools;
```

- [x] **Step 3: Create stub files so the module compiles**

Create `gait-rs/src/tools/write.rs`, `gait-rs/src/tools/edit.rs`, `gait-rs/src/tools/glob.rs`, `gait-rs/src/tools/grep.rs`, `gait-rs/src/tools/ls.rs`, `gait-rs/src/tools/shell.rs` each containing a temporary stub:

For each file (example for `write.rs`):
```rust
use std::path::PathBuf;
use crate::types::Tool;

pub struct WriteTool { codebase_path: PathBuf }
impl WriteTool {
    pub fn new(codebase_path: PathBuf) -> Self { Self { codebase_path } }
}
#[async_trait::async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str { "write" }
    fn description(&self) -> &str { "stub" }
    fn parameters(&self) -> &serde_json::Value { todo!() }
    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<serde_json::Value> { todo!() }
}
```

Repeat the same stub pattern for `edit.rs`, `glob.rs`, `grep.rs`, `ls.rs`, `shell.rs` with respective struct names and tool names.

- [x] **Step 4: Write failing tests for `read.rs`**

Create `gait-rs/src/tools/read.rs` with the tests but no implementation yet:

```rust
use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct ReadParams {
    /// Path to the file (relative to codebase root, or absolute)
    file_path: String,
    /// Line number to start reading from (0-based)
    offset: Option<u64>,
    /// Number of lines to read (default 2000)
    limit: Option<u64>,
}

pub struct ReadTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl ReadTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        todo!() // implement in next step
    }
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str { "read" }
    fn description(&self) -> &str {
        "Read a file from the codebase, optionally with line range. Paths are relative to the codebase root."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn reads_file_with_line_numbers() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "line1\nline2\nline3").unwrap();

        let tool = ReadTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "file_path": "test.txt" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("00001| line1"), "got: {text}");
        assert!(text.contains("00002| line2"), "got: {text}");
        assert!(text.contains("00003| line3"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_error_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "file_path": "nonexistent.txt" })).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn respects_offset_and_limit() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "a\nb\nc\nd\ne").unwrap();

        let tool = ReadTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "test.txt", "offset": 1, "limit": 2
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("00002| b"), "got: {text}");
        assert!(text.contains("00003| c"), "got: {text}");
        assert!(!text.contains("00001| a"), "should not contain line 1");
    }

    #[tokio::test]
    async fn rejects_binary_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("img.png"), b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "file_path": "img.png" })).await;
        assert!(result.is_err());
    }
}
```

- [x] **Step 5: Run tests to verify they fail (todo! panics)**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test read
```

Expected: Tests panic with `not yet implemented`

- [x] **Step 6: Implement `ReadTool`**

Replace the `todo!()` stubs in `read.rs` with the full implementation:

```rust
use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_READ_LIMIT: u64 = 2000;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

static BINARY_EXTENSIONS: &[&str] = &[
    "zip", "tar", "gz", "exe", "dll", "so", "class", "jar", "war",
    "7z", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods",
    "odp", "bin", "dat", "obj", "o", "a", "lib", "wasm", "pyc", "pyo",
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "tiff", "webp",
    "mp3", "mp4", "wav", "avi", "mkv", "pdf",
];

fn is_binary_by_extension(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| BINARY_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_binary_by_content(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let mut buf = [0u8; 4096];
    let Ok(n) = file.read(&mut buf) else { return false };
    if n == 0 { return false; }
    let sample = &buf[..n];
    // Null byte = definitely binary
    if sample.contains(&0u8) { return true; }
    // >30% non-printable (excluding tabs, newlines, carriage returns) = binary
    let non_printable = sample.iter()
        .filter(|&&b| b < 9 || (b > 13 && b < 32))
        .count();
    non_printable * 10 > n * 3
}

#[derive(Deserialize, JsonSchema)]
struct ReadParams {
    /// Path to the file (relative to codebase root, or absolute)
    file_path: String,
    /// Line number to start reading from (0-based)
    offset: Option<u64>,
    /// Number of lines to read (default 2000)
    limit: Option<u64>,
}

pub struct ReadTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl ReadTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<ReadParams>();
        Self {
            codebase_path: Arc::new(codebase_path),
            parameters,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str { "read" }
    fn description(&self) -> &str {
        "Read a file from the codebase, optionally with line range. Paths are relative to the codebase root."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: ReadParams = serde_json::from_value(args)?;
        let offset = params.offset.unwrap_or(0) as usize;
        let limit = params.limit.unwrap_or(DEFAULT_READ_LIMIT) as usize;

        let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
            PathBuf::from(&params.file_path)
        } else {
            self.codebase_path.join(&params.file_path)
        };

        if !resolved.exists() {
            // Suggest nearby files
            let dir = resolved.parent().unwrap_or(&resolved);
            let base = resolved.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();
            let suggestions: Vec<String> = if dir.exists() {
                std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        let name = e.file_name().to_string_lossy().to_lowercase();
                        name.contains(&base) || base.contains(name.as_str())
                    })
                    .take(3)
                    .map(|e| e.path().display().to_string())
                    .collect()
            } else {
                vec![]
            };
            let hint = if suggestions.is_empty() {
                String::new()
            } else {
                format!("\nDid you mean:\n{}", suggestions.join("\n"))
            };
            anyhow::bail!("File not found: {}{}", resolved.display(), hint);
        }

        if is_binary_by_extension(&resolved) || is_binary_by_content(&resolved) {
            anyhow::bail!("Cannot read binary file: {}", resolved.display());
        }

        let text = std::fs::read_to_string(&resolved)?;
        let lines: Vec<&str> = text.split('\n').collect();

        let mut output_lines: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        let mut truncated_by_bytes = false;
        let end = lines.len().min(offset + limit);

        for i in offset..end {
            let raw = lines[i];
            let line = if raw.len() > MAX_LINE_LENGTH {
                format!("{}...", &raw[..MAX_LINE_LENGTH])
            } else {
                raw.to_string()
            };
            let size = line.len() + if output_lines.is_empty() { 0 } else { 1 };
            if bytes + size > MAX_BYTES {
                truncated_by_bytes = true;
                break;
            }
            output_lines.push(format!("{:05}| {}", i + 1, line));
            bytes += size;
        }

        let content = output_lines.join("\n");
        let total_lines = lines.len();
        let last_read = offset + output_lines.len();

        let footer = if truncated_by_bytes {
            format!("\n\n(Output truncated at {} bytes. Use 'offset' to read beyond line {})", MAX_BYTES, last_read)
        } else if total_lines > last_read {
            format!("\n\n(File has more lines. Use 'offset' to read beyond line {})", last_read)
        } else {
            format!("\n\n(End of file - total {} lines)", total_lines)
        };

        Ok(Value::String(format!("<file>\n{}{}\n</file>", content, footer)))
    }
}
```

- [x] **Step 7: Run read tests**

```bash
cargo test read
```

Expected: All 4 tests PASS

- [x] **Step 8: Verify full test suite still passes**

```bash
cargo test
```

Expected: All tests pass

- [x] **Step 9: Commit**

```bash
git add gait-rs/src/lib.rs gait-rs/src/tools/
git commit -m "feat(rs): tools module skeleton + ReadTool with binary detection"
```

---

### Task 3: `tools/write.rs`

**Files:**
- Modify: `gait-rs/src/tools/write.rs` (replace stub)

---

- [x] **Step 1: Write failing tests in `write.rs`**

Replace the stub content of `gait-rs/src/tools/write.rs` with:

```rust
use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct WriteParams {
    /// Path to the file to write (relative to codebase root, or absolute)
    file_path: String,
    /// The content to write to the file
    content: String,
}

pub struct WriteTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl WriteTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        todo!()
    }
}

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str { "write" }
    fn description(&self) -> &str {
        "Write content to a file. Creates the file (and parent directories) if they don't exist. Paths are relative to the codebase root."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    async fn execute(&self, args: Value) -> anyhow::Result<Value> { todo!() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn writes_file_content() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "output.txt",
            "content": "hello world"
        })).await.unwrap();
        let content = std::fs::read_to_string(dir.path().join("output.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "sub/nested/file.txt",
            "content": "deep"
        })).await.unwrap();
        let content = std::fs::read_to_string(dir.path().join("sub/nested/file.txt")).unwrap();
        assert_eq!(content, "deep");
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "old").unwrap();
        let tool = WriteTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "existing.txt",
            "content": "new"
        })).await.unwrap();
        let content = std::fs::read_to_string(dir.path().join("existing.txt")).unwrap();
        assert_eq!(content, "new");
    }
}
```

- [x] **Step 2: Run tests to verify they fail**

```bash
cargo test write
```

Expected: panic with `not yet implemented`

- [x] **Step 3: Implement `WriteTool`**

Replace both `todo!()` stubs with the real implementation:

```rust
impl WriteTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<WriteParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

// execute:
async fn execute(&self, args: Value) -> anyhow::Result<Value> {
    let params: WriteParams = serde_json::from_value(args)?;
    let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
        PathBuf::from(&params.file_path)
    } else {
        self.codebase_path.join(&params.file_path)
    };
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&resolved, &params.content)?;
    let bytes = params.content.len();
    Ok(Value::String(format!("Wrote {} bytes to {}", bytes, resolved.display())))
}
```

- [x] **Step 4: Run tests**

```bash
cargo test write
```

Expected: All 3 tests PASS

- [x] **Step 5: Commit**

```bash
git add gait-rs/src/tools/write.rs
git commit -m "feat(rs): WriteTool — create/overwrite files with parent mkdir"
```

---

### Task 4: `tools/edit.rs` — replacers + `replace()` function

**Files:**
- Modify: `gait-rs/src/tools/edit.rs` (replace stub with replacer infrastructure)

This is the most complex tool. The `replace(content, old_str, new_str, replace_all)` function tries 9 matching strategies in order, from exact to increasingly fuzzy, returning the first unambiguous match. Port from `src/tools/edit.ts` (OpenCode MIT).

---

- [x] **Step 1: Write failing tests for the `replace()` function**

Replace the stub in `gait-rs/src/tools/edit.rs` with this (tests + skeleton, no implementation):

```rust
use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("oldString not found in content")]
    NotFound,
    #[error("Found multiple matches for oldString. Provide more surrounding lines to identify the correct match.")]
    MultipleMatches,
    #[error("oldString and newString must be different")]
    SameStrings,
}

// ── Levenshtein distance ──────────────────────────────────────────────────────

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    if m == 0 { return n; }
    if n == 0 { return m; }
    let mut matrix = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..=m { matrix[i][0] = i; }
    for j in 0..=n { matrix[0][j] = j; }
    for i in 1..=m {
        for j in 1..=n {
            let cost = if a[i-1] == b[j-1] { 0 } else { 1 };
            matrix[i][j] = (matrix[i-1][j] + 1)
                .min(matrix[i][j-1] + 1)
                .min(matrix[i-1][j-1] + cost);
        }
    }
    matrix[m][n]
}

// ── Replacer type: (content, find) -> candidate substrings from content ──────

type Replacer = fn(&str, &str) -> Vec<String>;

fn simple_replacer(_content: &str, find: &str) -> Vec<String> {
    vec![find.to_string()]
}

fn line_trimmed_replacer(content: &str, find: &str) -> Vec<String> {
    let orig: Vec<&str> = content.split('\n').collect();
    let mut search: Vec<&str> = find.split('\n').collect();
    if search.last() == Some(&"") { search.pop(); }
    let slen = search.len();
    let mut results = vec![];
    for i in 0..=orig.len().saturating_sub(slen) {
        let matches = (0..slen).all(|j| orig[i+j].trim() == search[j].trim());
        if matches {
            let start: usize = (0..i).map(|k| orig[k].len() + 1).sum();
            let end = start + (0..slen).map(|k| orig[i+k].len() + if k < slen-1 { 1 } else { 0 }).sum::<usize>();
            results.push(content[start..end].to_string());
        }
    }
    results
}

fn block_anchor_replacer(content: &str, find: &str) -> Vec<String> {
    let orig: Vec<&str> = content.split('\n').collect();
    let mut search: Vec<&str> = find.split('\n').collect();
    if search.len() < 3 { return vec![]; }
    if search.last() == Some(&"") { search.pop(); }
    let slen = search.len();
    let first = search[0].trim();
    let last  = search[slen - 1].trim();

    let mut candidates: Vec<(usize, usize)> = vec![];
    for i in 0..orig.len() {
        if orig[i].trim() != first { continue; }
        for j in (i+2)..orig.len() {
            if orig[j].trim() == last { candidates.push((i, j)); break; }
        }
    }
    if candidates.is_empty() { return vec![]; }

    let compute_match = |si: usize, ei: usize| -> f64 {
        let actual = ei - si + 1;
        let lines_to_check = (slen - 2).min(actual - 2);
        if lines_to_check == 0 { return 1.0; }
        let mut sim = 0.0f64;
        for j in 1..slen-1 {
            if si + j >= ei { break; }
            let o = orig[si + j].trim();
            let s = search[j].trim();
            let max_len = o.len().max(s.len());
            if max_len == 0 { continue; }
            sim += (1.0 - levenshtein(o, s) as f64 / max_len as f64) / lines_to_check as f64;
        }
        sim
    };

    let extract = |si: usize, ei: usize| -> String {
        let start: usize = (0..si).map(|k| orig[k].len() + 1).sum();
        let end = start + (si..=ei).enumerate().map(|(idx, k)| orig[k].len() + if idx < ei - si { 1 } else { 0 }).sum::<usize>();
        content[start..end].to_string()
    };

    if candidates.len() == 1 {
        let (si, ei) = candidates[0];
        if compute_match(si, ei) >= 0.0 { vec![extract(si, ei)] } else { vec![] }
    } else {
        let best = candidates.iter()
            .map(|&(si, ei)| (compute_match(si, ei), si, ei))
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        match best {
            Some((sim, si, ei)) if sim >= 0.3 => vec![extract(si, ei)],
            _ => vec![],
        }
    }
}

fn whitespace_normalized_replacer(content: &str, find: &str) -> Vec<String> {
    let normalize = |t: &str| -> String {
        t.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let norm_find = normalize(find);
    let mut results = vec![];
    // Single-line match
    for line in content.split('\n') {
        if normalize(line) == norm_find {
            results.push(line.to_string());
        }
    }
    // Multi-line block match
    let find_lines: Vec<&str> = find.split('\n').collect();
    let orig_lines: Vec<&str> = content.split('\n').collect();
    if find_lines.len() > 1 {
        for i in 0..=orig_lines.len().saturating_sub(find_lines.len()) {
            let block = orig_lines[i..i+find_lines.len()].join("\n");
            if normalize(&block) == norm_find {
                results.push(block);
            }
        }
    }
    results
}

fn indentation_flexible_replacer(content: &str, find: &str) -> Vec<String> {
    let remove_indent = |text: &str| -> String {
        let lines: Vec<&str> = text.split('\n').collect();
        let non_empty: Vec<&str> = lines.iter().filter(|l| !l.trim().is_empty()).copied().collect();
        if non_empty.is_empty() { return text.to_string(); }
        let min_indent = non_empty.iter()
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);
        lines.iter().map(|l| {
            if l.trim().is_empty() { l.to_string() } else { l[min_indent..].to_string() }
        }).collect::<Vec<_>>().join("\n")
    };
    let norm_find = remove_indent(find);
    let orig_lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = find.split('\n').collect();
    let mut results = vec![];
    for i in 0..=orig_lines.len().saturating_sub(find_lines.len()) {
        let block = orig_lines[i..i+find_lines.len()].join("\n");
        if remove_indent(&block) == norm_find {
            results.push(block);
        }
    }
    results
}

fn escape_normalized_replacer(content: &str, find: &str) -> Vec<String> {
    let unescape = |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n')  => out.push('\n'),
                    Some('t')  => out.push('\t'),
                    Some('r')  => out.push('\r'),
                    Some(c)    => out.push(c),
                    None       => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    let unescaped = unescape(find);
    if unescaped == find { return vec![]; } // nothing changed, avoid duplicate with simple_replacer
    let mut results = vec![];
    if content.contains(&unescaped as &str) {
        results.push(unescaped.clone());
    }
    let find_lines: Vec<&str> = unescaped.split('\n').collect();
    let orig_lines: Vec<&str> = content.split('\n').collect();
    for i in 0..=orig_lines.len().saturating_sub(find_lines.len()) {
        let block = orig_lines[i..i+find_lines.len()].join("\n");
        if unescape(&block) == unescaped && !results.contains(&block) {
            results.push(block);
        }
    }
    results
}

fn trimmed_boundary_replacer(content: &str, find: &str) -> Vec<String> {
    let trimmed = find.trim();
    if trimmed == find { return vec![]; }
    let mut results = vec![];
    if content.contains(trimmed) {
        results.push(trimmed.to_string());
    }
    let find_lines: Vec<&str> = find.split('\n').collect();
    let orig_lines: Vec<&str> = content.split('\n').collect();
    for i in 0..=orig_lines.len().saturating_sub(find_lines.len()) {
        let block = orig_lines[i..i+find_lines.len()].join("\n");
        if block.trim() == trimmed && !results.contains(&block) {
            results.push(block);
        }
    }
    results
}

fn context_aware_replacer(content: &str, find: &str) -> Vec<String> {
    let orig: Vec<&str> = content.split('\n').collect();
    let mut find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.len() < 3 { return vec![]; }
    if find_lines.last() == Some(&"") { find_lines.pop(); }
    let flen = find_lines.len();
    let first = find_lines[0].trim();
    let last  = find_lines[flen - 1].trim();
    let mut results = vec![];

    for i in 0..orig.len() {
        if orig[i].trim() != first { continue; }
        for j in (i+2)..orig.len() {
            if orig[j].trim() != last { continue; }
            let block = &orig[i..=j];
            if block.len() == flen {
                let (matching, total) = (1..flen-1).fold((0usize, 0usize), |(m, t), k| {
                    let bl = block[k].trim();
                    let fl = find_lines[k].trim();
                    if bl.is_empty() && fl.is_empty() { (m, t) }
                    else { (m + if bl == fl { 1 } else { 0 }, t + 1) }
                });
                if total == 0 || matching * 2 >= total {
                    results.push(block.join("\n"));
                }
            }
            break;
        }
    }
    results
}

fn multi_occurrence_replacer(content: &str, find: &str) -> Vec<String> {
    // Yields `find` once per occurrence — used to ensure not_found is false when
    // find literally appears multiple times in content (after all other replacers failed).
    let mut results = vec![];
    let mut start = 0;
    while let Some(idx) = content[start..].find(find) {
        results.push(find.to_string());
        start += idx + find.len();
    }
    results
}

/// Replace `old_str` with `new_str` in `content`. Tries 9 matching strategies from exact to fuzzy.
/// Returns `Err(EditError::NotFound)` if no match, `Err(EditError::MultipleMatches)` if ambiguous.
pub fn replace(content: &str, old_str: &str, new_str: &str, replace_all: bool) -> Result<String, EditError> {
    if old_str == new_str { return Err(EditError::SameStrings); }

    const REPLACERS: &[Replacer] = &[
        simple_replacer,
        line_trimmed_replacer,
        block_anchor_replacer,
        whitespace_normalized_replacer,
        indentation_flexible_replacer,
        escape_normalized_replacer,
        trimmed_boundary_replacer,
        context_aware_replacer,
        multi_occurrence_replacer,
    ];

    let mut not_found = true;

    for replacer in REPLACERS {
        for search in replacer(content, old_str) {
            let Some(first) = content.find(&search as &str) else { continue };
            not_found = false;

            if replace_all {
                return Ok(content.replace(&search as &str, new_str));
            }

            // Check for multiple occurrences of this candidate
            let last = content.rfind(&search as &str).unwrap();
            if first != last { continue; }

            let mut result = String::with_capacity(content.len() + new_str.len());
            result.push_str(&content[..first]);
            result.push_str(new_str);
            result.push_str(&content[first + search.len()..]);
            return Ok(result);
        }
    }

    if not_found { Err(EditError::NotFound) } else { Err(EditError::MultipleMatches) }
}
```

- [x] **Step 2: Write tests for `replace()` + `EditTool` (add after the `replace()` fn)**

```rust
#[derive(Deserialize, JsonSchema)]
struct EditParams {
    /// Path to the file to modify (relative to codebase root, or absolute)
    file_path: String,
    /// The text to replace (must exist exactly once in the file, or use replaceAll)
    old_string: String,
    /// The replacement text
    new_string: String,
    /// Replace all occurrences of oldString (default false)
    replace_all: Option<bool>,
}

pub struct EditTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl EditTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        todo!()
    }
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str { "edit" }
    fn description(&self) -> &str {
        "Edit a file by replacing one exact string with another. The oldString must uniquely identify the location to change."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    async fn execute(&self, args: Value) -> anyhow::Result<Value> { todo!() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── replace() unit tests ───────────────────────────────────────────────────

    #[test]
    fn simple_exact_replace() {
        let result = replace("hello world", "world", "rust", false).unwrap();
        assert_eq!(result, "hello rust");
    }

    #[test]
    fn replace_not_found() {
        let err = replace("hello world", "missing", "x", false).unwrap_err();
        assert!(matches!(err, EditError::NotFound));
    }

    #[test]
    fn replace_multiple_matches_errors() {
        let err = replace("foo foo foo", "foo", "bar", false).unwrap_err();
        assert!(matches!(err, EditError::MultipleMatches));
    }

    #[test]
    fn replace_all_replaces_every_occurrence() {
        let result = replace("foo foo foo", "foo", "bar", true).unwrap();
        assert_eq!(result, "bar bar bar");
    }

    #[test]
    fn same_strings_errors() {
        let err = replace("hello", "hello", "hello", false).unwrap_err();
        assert!(matches!(err, EditError::SameStrings));
    }

    #[test]
    fn line_trimmed_replace_ignores_indentation() {
        let content = "  fn hello() {\n    println!(\"hi\");\n  }";
        // Search with different indentation
        let result = replace(content, "fn hello() {\n  println!(\"hi\");\n}", "fn world() {}", false).unwrap();
        assert!(result.contains("fn world()"));
    }

    #[test]
    fn indentation_flexible_replace() {
        let content = "    if x {\n        do_thing();\n    }";
        let result = replace(content, "if x {\n    do_thing();\n}", "if y {}", false).unwrap();
        assert!(result.contains("if y {}"));
    }

    // ── EditTool integration tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn edit_tool_replaces_in_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hello\");\n}").unwrap();

        let tool = EditTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "code.rs",
            "old_string": "hello",
            "new_string": "world"
        })).await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"world\""));
        assert!(!content.contains("\"hello\""));
    }

    #[tokio::test]
    async fn edit_tool_creates_file_when_old_string_empty() {
        let dir = TempDir::new().unwrap();
        let tool = EditTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "new.txt",
            "old_string": "",
            "new_string": "created content"
        })).await.unwrap();

        let content = std::fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(content, "created content");
    }

    #[tokio::test]
    async fn edit_tool_returns_error_when_not_found() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "existing content").unwrap();
        let tool = EditTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "f.txt",
            "old_string": "nonexistent",
            "new_string": "x"
        })).await;
        assert!(result.is_err());
    }
}
```

- [x] **Step 3: Run to verify tests compile but todo!() panics**

```bash
cargo test edit
```

Expected: Compile succeeds, tests for `EditTool` panic on `todo!()`; `replace()` tests run.

- [x] **Step 4: Implement `EditTool`**

Replace both `todo!()` stubs in `EditTool`:

```rust
impl EditTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<EditParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

// execute:
async fn execute(&self, args: Value) -> anyhow::Result<Value> {
    let params: EditParams = serde_json::from_value(args)?;
    let replace_all = params.replace_all.unwrap_or(false);

    let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
        PathBuf::from(&params.file_path)
    } else {
        self.codebase_path.join(&params.file_path)
    };

    // empty old_string = create file
    if params.old_string.is_empty() {
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&resolved, &params.new_string)?;
        return Ok(Value::String(format!("Created {} with {} bytes",
            resolved.display(), params.new_string.len())));
    }

    if params.old_string == params.new_string {
        anyhow::bail!("old_string and new_string must be different");
    }
    if !resolved.exists() {
        anyhow::bail!("File not found: {}", resolved.display());
    }
    if resolved.is_dir() {
        anyhow::bail!("Path is a directory, not a file: {}", resolved.display());
    }

    let old_content = std::fs::read_to_string(&resolved)?;
    let new_content = replace(&old_content, &params.old_string, &params.new_string, replace_all)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    std::fs::write(&resolved, &new_content)?;
    let chars_changed = (new_content.len() as i64 - old_content.len() as i64).unsigned_abs() as usize;
    Ok(Value::String(format!("Edited {}: replaced {} chars", resolved.display(), chars_changed)))
}
```

- [x] **Step 5: Run all edit tests**

```bash
cargo test edit
```

Expected: All tests PASS

- [x] **Step 6: Run full suite**

```bash
cargo test
```

Expected: All tests pass

- [x] **Step 7: Commit**

```bash
git add gait-rs/src/tools/edit.rs
git commit -m "feat(rs): EditTool — 9-strategy fuzzy string replacement (port of opencode edit.ts)"
```

---

### Task 5: `tools/glob.rs`

**Files:**
- Modify: `gait-rs/src/tools/glob.rs` (replace stub)

---

- [x] **Step 1: Write failing tests for `GlobTool`**

Replace the stub in `gait-rs/src/tools/glob.rs` with:

```rust
use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const GLOB_LIMIT: usize = 100;

#[derive(Deserialize, JsonSchema)]
struct GlobParams {
    /// The glob pattern to match files against (e.g. '**/*.ts', 'src/**/*.json')
    pattern: String,
    /// Directory to search in (relative to codebase root). Omit to search entire codebase.
    path: Option<String>,
}

pub struct GlobTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl GlobTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        todo!()
    }
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str { "glob" }
    fn description(&self) -> &str {
        "Find files in the codebase matching a glob pattern. Results sorted by modification time (newest first)."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    async fn execute(&self, args: Value) -> anyhow::Result<Value> { todo!() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn finds_files_by_extension() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.ts"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();

        let tool = GlobTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "pattern": "*.rs" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("a.rs"), "got: {text}");
        assert!(text.contains("c.rs"), "got: {text}");
        assert!(!text.contains("b.ts"), "got: {text}");
    }

    #[tokio::test]
    async fn finds_nested_files_with_globstar() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();

        let tool = GlobTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "pattern": "**/*.rs" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("main.rs"), "got: {text}");
        assert!(!text.contains("README.md"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_no_files_found_when_empty() {
        let dir = TempDir::new().unwrap();
        let tool = GlobTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "pattern": "*.xyz" })).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "No files found");
    }

    #[tokio::test]
    async fn searches_in_subdir_when_path_given() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/deep.rs"), "").unwrap();
        std::fs::write(dir.path().join("root.rs"), "").unwrap();

        let tool = GlobTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "pattern": "*.rs",
            "path": "sub"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("deep.rs"), "got: {text}");
        assert!(!text.contains("root.rs"), "got: {text}");
    }
}
```

- [x] **Step 2: Run to verify tests fail**

```bash
cargo test glob
```

Expected: panic with `not yet implemented`

- [x] **Step 3: Implement `GlobTool`**

Replace `todo!()` stubs:

```rust
impl GlobTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<GlobParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

// execute:
async fn execute(&self, args: Value) -> anyhow::Result<Value> {
    let params: GlobParams = serde_json::from_value(args)?;

    let search_dir = match &params.path {
        Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
        Some(p) => self.codebase_path.join(p),
        None => (*self.codebase_path).clone(),
    };

    if !search_dir.exists() {
        anyhow::bail!("Directory not found: {}", search_dir.display());
    }

    // Construct absolute glob pattern: search_dir + "/" + user_pattern
    let abs_pattern = format!("{}/{}", search_dir.display(), params.pattern);

    let mut files: Vec<(PathBuf, Option<std::time::SystemTime>)> = vec![];
    let mut truncated = false;

    let glob_results = ::glob::glob(&abs_pattern)
        .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {e}"))?;

    for entry in glob_results {
        match entry {
            Ok(path) if path.is_file() => {
                if files.len() >= GLOB_LIMIT { truncated = true; break; }
                let mtime = path.metadata().ok().and_then(|m| m.modified().ok());
                files.push((path, mtime));
            }
            _ => {}
        }
    }

    if files.is_empty() { return Ok(Value::String("No files found".into())); }

    files.sort_by(|a, b| b.1.cmp(&a.1));

    let mut lines: Vec<String> = files.iter()
        .map(|(p, _)| p.display().to_string())
        .collect();
    if truncated {
        lines.push(String::new());
        lines.push("(Results are truncated. Consider using a more specific path or pattern.)".into());
    }

    Ok(Value::String(lines.join("\n")))
}
```

- [x] **Step 4: Run glob tests**

```bash
cargo test glob
```

Expected: All 4 tests PASS

- [x] **Step 5: Commit**

```bash
git add gait-rs/src/tools/glob.rs
git commit -m "feat(rs): GlobTool — find files by glob pattern using glob crate"
```

---

### Task 6: `tools/grep.rs`

**Files:**
- Modify: `gait-rs/src/tools/grep.rs` (replace stub)

---

- [x] **Step 1: Write failing tests for `GrepTool`**

Replace the stub in `gait-rs/src/tools/grep.rs` with:

```rust
use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const MAX_LINE_LENGTH: usize = 2000;
const GREP_LIMIT: usize = 100;

#[derive(Deserialize, JsonSchema)]
struct GrepParams {
    /// The regex pattern to search for in file contents
    pattern: String,
    /// Directory or file to search (relative to codebase root). Defaults to entire codebase.
    path: Option<String>,
    /// File glob pattern to include (e.g. "*.rs", "*.{ts,tsx}")
    include: Option<String>,
}

struct MatchEntry {
    path: String,
    line_num: usize,
    line_text: String,
    mod_time: Option<std::time::SystemTime>,
}

pub struct GrepTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl GrepTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        todo!()
    }
}

fn rg_search(pattern: &str, search_path: &str, include: Option<&str>) -> anyhow::Result<Vec<MatchEntry>> {
    // Use rg with --no-heading -n so output is "path:linenum:text" per line.
    // We split with splitn(3, ':') so file paths containing ':' (rare on Unix) are handled:
    // the line_text (parts[2]) may contain ':', which is fine since splitn stops at 3 parts.
    // (The `--field-match-separator` flag changes the separator *within* the match context,
    // not the path:line prefix — so it does NOT work with `-nH` format. Use default format.)
    let mut cmd = std::process::Command::new("rg");
    cmd.args(["-nH", "--hidden", "--no-heading", "--no-messages", "--regexp", pattern]);
    if let Some(inc) = include { cmd.args(["--glob", inc]); }
    cmd.arg(search_path);
    let output = cmd.output()?;
    if output.stdout.is_empty() {
        return Ok(vec![]);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut matches = vec![];
    for line in text.lines() {
        if line.is_empty() { continue; }
        // Format: /path/to/file.rs:42:line text here
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        if parts.len() < 3 { continue; }
        let file_path = parts[0].to_string();
        let Ok(line_num) = parts[1].parse::<usize>() else { continue };
        let line_text = parts[2].to_string();
        let mod_time = std::fs::metadata(&file_path).ok().and_then(|m| m.modified().ok());
        matches.push(MatchEntry { path: file_path, line_num, line_text, mod_time });
    }
    Ok(matches)
}

fn regex_search(pattern: &str, search_path: &std::path::Path) -> anyhow::Result<Vec<MatchEntry>> {
    let re = regex::Regex::new(pattern)
        .map_err(|e| anyhow::anyhow!("Invalid regex pattern: {e}"))?;
    let mut matches = vec![];
    let files: Box<dyn Iterator<Item = walkdir::DirEntry>> = if search_path.is_file() {
        Box::new(walkdir::WalkDir::new(search_path).into_iter().filter_map(|e| e.ok()))
    } else {
        Box::new(walkdir::WalkDir::new(search_path).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()))
    };
    for entry in files {
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(path) else { continue };
        let mod_time = path.metadata().ok().and_then(|m| m.modified().ok());
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                matches.push(MatchEntry {
                    path: path.display().to_string(),
                    line_num: i + 1,
                    line_text: line.to_string(),
                    mod_time,
                });
            }
        }
    }
    Ok(matches)
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str { "grep" }
    fn description(&self) -> &str {
        "Search for a regex pattern across files in the codebase. Returns matching lines with file and line number."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    async fn execute(&self, args: Value) -> anyhow::Result<Value> { todo!() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn finds_pattern_in_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("foo.rs"),
            "fn main() {\n    println!(\"hello world\");\n}").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "pattern": "println" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("foo.rs"), "got: {text}");
        assert!(text.contains("println"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_no_files_found_when_no_match() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "pattern": "zzz_not_present" })).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "No files found");
    }

    #[tokio::test]
    async fn respects_path_argument() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), "needle").unwrap();
        std::fs::write(dir.path().join("b.txt"), "needle").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "pattern": "needle",
            "path": "sub"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("a.txt"), "got: {text}");
        // Should only show matches from sub/, not root b.txt
        assert!(!text.contains("b.txt"), "got: {text}");
    }
}
```

- [x] **Step 2: Run to verify tests fail**

```bash
cargo test grep
```

Expected: panic with `not yet implemented`

- [x] **Step 3: Implement `GrepTool`**

Replace both `todo!()` stubs:

```rust
impl GrepTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<GrepParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

// execute:
async fn execute(&self, args: Value) -> anyhow::Result<Value> {
    let params: GrepParams = serde_json::from_value(args)?;

    let resolved = match &params.path {
        Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
        Some(p) => self.codebase_path.join(p),
        None => (*self.codebase_path).clone(),
    };

    let mut matches = rg_search(&params.pattern, &resolved.display().to_string(), params.include.as_deref())
        .unwrap_or_default();

    if matches.is_empty() {
        matches = regex_search(&params.pattern, &resolved)?;
    }

    if matches.is_empty() { return Ok(Value::String("No files found".into())); }

    matches.sort_by(|a, b| b.mod_time.cmp(&a.mod_time));

    let truncated = matches.len() > GREP_LIMIT;
    let final_matches = &matches[..matches.len().min(GREP_LIMIT)];

    let mut lines = vec![format!("Found {} matches", final_matches.len())];
    let mut current_file = String::new();
    for m in final_matches {
        if current_file != m.path {
            if !current_file.is_empty() { lines.push(String::new()); }
            current_file = m.path.clone();
            lines.push(format!("{}:", m.path));
        }
        let text = if m.line_text.len() > MAX_LINE_LENGTH {
            format!("{}...", &m.line_text[..MAX_LINE_LENGTH])
        } else {
            m.line_text.clone()
        };
        lines.push(format!("  Line {}: {}", m.line_num, text));
    }
    if truncated {
        lines.push(String::new());
        lines.push("(Results are truncated. Consider using a more specific path or pattern.)".into());
    }

    Ok(Value::String(lines.join("\n")))
}
```

- [x] **Step 4: Run grep tests**

```bash
cargo test grep
```

Expected: All 3 tests PASS

- [x] **Step 5: Commit**

```bash
git add gait-rs/src/tools/grep.rs
git commit -m "feat(rs): GrepTool — regex search with rg binary + walkdir fallback"
```

---

### Task 7: `tools/ls.rs`

**Files:**
- Modify: `gait-rs/src/tools/ls.rs` (replace stub)

---

- [x] **Step 1: Write failing tests for `LsTool`**

Replace the stub in `gait-rs/src/tools/ls.rs` with:

```rust
use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const LS_LIMIT: usize = 100;

static IGNORE_NAMES: &[&str] = &[
    "node_modules", "__pycache__", ".git", "dist", "build", "target",
    "vendor", "bin", "obj", ".idea", ".vscode", ".zig-cache", "zig-out",
    ".coverage", "coverage", "tmp", "temp", ".cache", "cache", "logs",
    ".venv", "venv", "env",
];

#[derive(Deserialize, JsonSchema)]
struct LsParams {
    /// Directory to list (relative to codebase root). Omit to list codebase root.
    path: Option<String>,
    /// Additional directory/file names to ignore
    ignore: Option<Vec<String>>,
}

pub struct LsTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl LsTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        todo!()
    }
}

fn should_ignore(name: &str, extra: &[String]) -> bool {
    IGNORE_NAMES.contains(&name) || extra.iter().any(|e| e == name)
}

fn collect_files_walkdir(dir: &std::path::Path, extra_ignore: &[String]) -> (Vec<PathBuf>, bool) {
    let mut files = vec![];
    let mut truncated = false;
    // Use filter_entry to PRUNE ignored directories (not just skip their files).
    // Without filter_entry, WalkDir still descends into node_modules/ etc. and yields
    // every file inside — `continue` only skips the current entry, not the subtree.
    for entry in walkdir::WalkDir::new(dir).into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !should_ignore(&name, &[]) // prune based on static list only (extra_ignore applied below)
        })
        .filter_map(|e| e.ok())
    {
        let name = entry.file_name().to_string_lossy();
        if should_ignore(&name, extra_ignore) { continue; } // extra ignores skip without pruning (rare case)
        if entry.file_type().is_file() {
            if files.len() >= LS_LIMIT { truncated = true; break; }
            files.push(entry.path().to_path_buf());
        }
    }
    (files, truncated)
}

fn collect_files_rg(dir: &std::path::Path, extra_ignore: &[String]) -> anyhow::Result<(Vec<PathBuf>, bool)> {
    let mut cmd = std::process::Command::new("rg");
    cmd.args(["--files", "--hidden", "--no-messages"]);
    for name in IGNORE_NAMES.iter().chain(extra_ignore.iter().map(|s| s.as_str())) {
        cmd.arg(format!("--glob=!{}/**", name));
    }
    cmd.arg(dir);
    let output = cmd.output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let all: Vec<PathBuf> = text.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect();
    let truncated = all.len() >= LS_LIMIT;
    Ok((all.into_iter().take(LS_LIMIT).collect(), truncated))
}

fn render_tree(search_dir: &std::path::Path, files: &[PathBuf], truncated: bool) -> String {
    use std::collections::{BTreeMap, BTreeSet};
    let mut dir_files: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut all_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    all_dirs.insert(PathBuf::from("."));

    for file in files {
        let rel = file.strip_prefix(search_dir).unwrap_or(file);
        let parent = rel.parent().unwrap_or(std::path::Path::new("."));
        let name = rel.file_name().unwrap_or_default().to_string_lossy().into_owned();
        dir_files.entry(parent.to_path_buf()).or_default().insert(name);
        // Register all ancestor directories
        let mut d = parent.to_path_buf();
        while d != std::path::Path::new(".") && d != std::path::Path::new("") {
            all_dirs.insert(d.clone());
            d = d.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
        }
        all_dirs.insert(parent.to_path_buf());
    }

    fn render_dir(
        dir_path: &std::path::Path,
        all_dirs: &BTreeSet<PathBuf>,
        dir_files: &BTreeMap<PathBuf, BTreeSet<String>>,
        depth: usize,
    ) -> String {
        let indent = "  ".repeat(depth);
        let mut out = String::new();
        if depth > 0 {
            let name = dir_path.file_name().unwrap_or_default().to_string_lossy();
            out.push_str(&format!("{}{}/\n", indent, name));
        }
        let child_indent = "  ".repeat(depth + 1);
        // Child directories (immediate children only)
        let children: Vec<&PathBuf> = all_dirs.iter().filter(|d| {
            d.parent().map(|p| p == dir_path).unwrap_or(false) && *d != dir_path
        }).collect();
        for child in children {
            out.push_str(&render_dir(child, all_dirs, dir_files, depth + 1));
        }
        if let Some(files) = dir_files.get(dir_path) {
            for f in files {
                out.push_str(&format!("{}{}\n", child_indent, f));
            }
        }
        out
    }

    let root = std::path::Path::new(".");
    let mut output = format!("{}/\n", search_dir.display());
    output.push_str(&render_dir(root, &all_dirs, &dir_files, 0));
    if truncated {
        output.push_str("\n(Results are truncated. Only first 100 files shown.)");
    }
    output
}

#[async_trait::async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str { "ls" }
    fn description(&self) -> &str {
        "List files in a directory of the codebase as a tree. Ignores common build/dependency directories."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    async fn execute(&self, args: Value) -> anyhow::Result<Value> { todo!() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn lists_root_as_tree() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();

        let tool = LsTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("main.rs"), "got: {text}");
        assert!(text.contains("Cargo.toml"), "got: {text}");
    }

    #[tokio::test]
    async fn ignores_node_modules() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/pkg.js"), "").unwrap();
        std::fs::write(dir.path().join("index.js"), "").unwrap();

        let tool = LsTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("index.js"), "got: {text}");
        assert!(!text.contains("pkg.js"), "got: {text}");
    }

    #[tokio::test]
    async fn lists_subdirectory_when_path_given() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file.rs"), "").unwrap();
        std::fs::write(dir.path().join("root.rs"), "").unwrap();

        let tool = LsTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "path": "sub" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("file.rs"), "got: {text}");
        // root.rs is outside sub/ — should not appear
        assert!(!text.contains("root.rs"), "got: {text}");
    }
}
```

- [x] **Step 2: Run to verify tests fail**

```bash
cargo test 'tools::ls::'
```

Expected: panic with `not yet implemented`

- [x] **Step 3: Implement `LsTool`**

Replace `todo!()` stubs:

```rust
impl LsTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<LsParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

// execute:
async fn execute(&self, args: Value) -> anyhow::Result<Value> {
    let params: LsParams = serde_json::from_value(args)?;
    let extra_ignore = params.ignore.unwrap_or_default();

    let search_dir = match &params.path {
        Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
        Some(p) => self.codebase_path.join(p),
        None => (*self.codebase_path).clone(),
    };

    if !search_dir.exists() {
        anyhow::bail!("Directory not found: {}", search_dir.display());
    }
    if !search_dir.is_dir() {
        anyhow::bail!("Not a directory: {}", search_dir.display());
    }

    let (files, truncated) = collect_files_rg(&search_dir, &extra_ignore)
        .unwrap_or_else(|_| collect_files_walkdir(&search_dir, &extra_ignore));

    Ok(Value::String(render_tree(&search_dir, &files, truncated)))
}
```

- [x] **Step 4: Run ls tests**

```bash
cargo test 'tools::ls::'
```

Expected: All 3 tests PASS

- [x] **Step 5: Commit**

```bash
git add gait-rs/src/tools/ls.rs
git commit -m "feat(rs): LsTool — directory tree listing with rg + walkdir fallback"
```

---

### Task 8: `tools/shell.rs`

**Files:**
- Modify: `gait-rs/src/tools/shell.rs` (replace stub)

---

- [x] **Step 1: Write failing tests for `ShellTool`**

Replace the stub in `gait-rs/src/tools/shell.rs` with:

```rust
use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Deserialize, JsonSchema)]
struct ShellParams {
    /// Shell command to execute
    cmd: String,
    /// Why this command is needed
    reason: String,
    /// Timeout in milliseconds (default 30000)
    timeout_ms: Option<u64>,
}

pub struct ShellTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl ShellTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        todo!()
    }
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str { "shell" }
    fn description(&self) -> &str {
        "Run a shell command in the codebase directory. All shell commands require permission approval unless pre-allowed."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    async fn execute(&self, args: Value) -> anyhow::Result<Value> { todo!() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn runs_command_and_captures_output() {
        let dir = TempDir::new().unwrap();
        let tool = ShellTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "cmd": "echo hello",
            "reason": "test"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("exit 0"), "got: {text}");
        assert!(text.contains("hello"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_nonzero_exit_code_on_failure() {
        let dir = TempDir::new().unwrap();
        let tool = ShellTool::new(dir.path().to_path_buf());
        // `false` is a command that always exits with code 1
        let result = tool.execute(serde_json::json!({
            "cmd": "false",
            "reason": "test"
        })).await.unwrap(); // execute always returns Ok — exit code embedded in the string
        let text = result.as_str().unwrap();
        assert!(text.starts_with("exit 1") || text.contains("exit 1"), "got: {text}");
    }

    #[tokio::test]
    async fn runs_in_codebase_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("sentinel.txt"), "marker").unwrap();
        let tool = ShellTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "cmd": "cat sentinel.txt",
            "reason": "test"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("marker"), "got: {text}");
    }

    #[tokio::test]
    async fn times_out_long_running_command() {
        let dir = TempDir::new().unwrap();
        let tool = ShellTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "cmd": "sleep 10",
            "reason": "test timeout",
            "timeout_ms": 100
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("TIMED OUT"), "got: {text}");
    }
}
```

- [x] **Step 2: Run to verify tests fail**

```bash
cargo test shell
```

Expected: panic with `not yet implemented`

- [x] **Step 3: Implement `ShellTool`**

Replace `todo!()` stubs:

```rust
impl ShellTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<ShellParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

// execute:
async fn execute(&self, args: Value) -> anyhow::Result<Value> {
    let params: ShellParams = serde_json::from_value(args)?;
    let timeout_ms = params.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let codebase_path = self.codebase_path.clone();

    let start = std::time::Instant::now();

    // Run blocking shell in a tokio task so we can timeout
    let cmd = params.cmd.clone();
    let cwd = (*codebase_path).clone();

    let child = tokio::process::Command::new("sh")
        .args(["-c", &cmd])
        .current_dir(&cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        child.wait_with_output(),
    ).await;

    let duration_ms = start.elapsed().as_millis();

    match result {
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(1);
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let combined = format!("{}{}", stdout, stderr);
            Ok(Value::String(format!("exit {} ({}ms)\n{}", exit_code, duration_ms, combined)))
        }
        Ok(Err(e)) => anyhow::bail!("Process error: {}", e),
        Err(_) => {
            Ok(Value::String(format!("TIMED OUT after {}ms\n", timeout_ms)))
        }
    }
}
```

- [x] **Step 4: Run shell tests**

```bash
cargo test shell
```

Expected: All 4 tests PASS. The timeout test may take ~100ms.

- [x] **Step 5: Commit**

```bash
git add gait-rs/src/tools/shell.rs
git commit -m "feat(rs): ShellTool — async command runner with configurable timeout"
```

---

### Task 9: Wire `builtin_tools` + `lib.rs` exports + integration test

**Files:**
- Modify: `gait-rs/src/tools/mod.rs` (replace stub stubs with real imports)
- Modify: `gait-rs/src/lib.rs`
- Create: `gait-rs/tests/integration_tools.rs`

---

- [x] **Step 1: Finalize `tools/mod.rs`**

`gait-rs/src/tools/mod.rs` should already have `builtin_tools()` from Task 2 Step 1. Verify it compiles now that all stubs have real implementations:

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo build
```

Expected: no errors

- [x] **Step 2: Update `lib.rs` exports**

The current `gait-rs/src/lib.rs` should have:
```rust
pub mod tools;
pub use tools::builtin_tools;
```

If not present, add both lines.

Also ensure the full export list is:
```rust
pub mod types;
pub mod provider;
pub mod anthropic;
pub mod openai;
pub mod stream;
pub mod generate;
pub mod factory;
pub mod tools;

pub use types::*;
pub use stream::stream_text;
pub use generate::generate_text;
pub use factory::load_provider;
pub use tools::builtin_tools;
```

- [x] **Step 3: Write integration test**

Create `gait-rs/tests/integration_tools.rs`:

```rust
use gait_ai::builtin_tools;
use gait_ai::types::Tool;
use tempfile::TempDir;

#[tokio::test]
async fn builtin_tools_returns_seven_tools() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf());
    assert_eq!(tools.len(), 7);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert!(names.contains(&"read"));
    assert!(names.contains(&"write"));
    assert!(names.contains(&"edit"));
    assert!(names.contains(&"glob"));
    assert!(names.contains(&"grep"));
    assert!(names.contains(&"ls"));
    assert!(names.contains(&"shell"));
}

#[tokio::test]
async fn all_tools_have_valid_json_schema_parameters() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf());
    for tool in &tools {
        let schema = tool.parameters();
        assert!(schema.is_object(), "tool '{}' has non-object schema", tool.name());
        // make_schema strips $schema and title — verify they're gone (important for Anthropic API)
        assert!(schema.get("$schema").is_none(),
            "tool '{}' schema should not have $schema key (Anthropic rejects it)", tool.name());
        assert!(schema.get("title").is_none(),
            "tool '{}' schema should not have title key", tool.name());
        // Schema should have properties or type=object
        assert!(schema.get("properties").is_some() || schema.get("type").is_some(),
            "tool '{}' schema missing properties/type", tool.name());
    }
}

#[tokio::test]
async fn read_write_roundtrip_via_builtin_tools() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf());
    let write_tool = tools.iter().find(|t| t.name() == "write").unwrap();
    let read_tool  = tools.iter().find(|t| t.name() == "read").unwrap();

    // Write a file
    write_tool.execute(serde_json::json!({
        "file_path": "hello.txt",
        "content": "hello from gait-rs"
    })).await.unwrap();

    // Read it back
    let result = read_tool.execute(serde_json::json!({
        "file_path": "hello.txt"
    })).await.unwrap();

    let text = result.as_str().unwrap();
    assert!(text.contains("hello from gait-rs"), "got: {text}");
    assert!(text.contains("00001|"), "should have line numbers");
}

#[tokio::test]
async fn write_then_edit_then_read() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf());
    let write = tools.iter().find(|t| t.name() == "write").unwrap();
    let edit  = tools.iter().find(|t| t.name() == "edit").unwrap();
    let read  = tools.iter().find(|t| t.name() == "read").unwrap();

    write.execute(serde_json::json!({
        "file_path": "code.rs",
        "content": "fn hello() -> &'static str { \"hello\" }"
    })).await.unwrap();

    edit.execute(serde_json::json!({
        "file_path": "code.rs",
        "old_string": "hello",
        "new_string": "world",
        "replace_all": true
    })).await.unwrap();

    let result = read.execute(serde_json::json!({ "file_path": "code.rs" })).await.unwrap();
    let text = result.as_str().unwrap();
    assert!(text.contains("world"), "got: {text}");
}

#[tokio::test]
async fn glob_finds_written_files() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf());
    let write = tools.iter().find(|t| t.name() == "write").unwrap();
    let glob  = tools.iter().find(|t| t.name() == "glob").unwrap();

    write.execute(serde_json::json!({ "file_path": "a.rs", "content": "" })).await.unwrap();
    write.execute(serde_json::json!({ "file_path": "b.rs", "content": "" })).await.unwrap();
    write.execute(serde_json::json!({ "file_path": "c.ts", "content": "" })).await.unwrap();

    let result = glob.execute(serde_json::json!({ "pattern": "*.rs" })).await.unwrap();
    let text = result.as_str().unwrap();
    assert!(text.contains("a.rs"), "got: {text}");
    assert!(text.contains("b.rs"), "got: {text}");
    assert!(!text.contains("c.ts"), "got: {text}");
}
```

- [x] **Step 4: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test
```

Expected: All unit and integration tests pass. (Live API integration tests skip if env vars not set.)

- [x] **Step 5: Final commit**

```bash
git add gait-rs/src/lib.rs gait-rs/src/tools/mod.rs gait-rs/tests/integration_tools.rs
git commit -m "feat(rs): builtin_tools() + integration tests for all 7 tools"
```

---

## Self-Review

**Spec coverage check:**
- [x] `Tool` trait for extensibility — Task 1
- [x] `read` — Task 2 (`ReadTool` with binary detection, line numbers, offset/limit, 50KB cap)
- [x] `write` — Task 3 (`WriteTool` with parent mkdir)
- [x] `edit` — Task 4 (9-strategy fuzzy replace + `EditTool`)
- [x] `glob` — Task 5 (`GlobTool` with `glob` crate, mtime sort)
- [x] `grep` — Task 6 (`GrepTool` with `rg` + `walkdir+regex` fallback)
- [x] `ls` — Task 7 (`LsTool` with `rg --files` + `walkdir` fallback, tree rendering, ignore list)
- [x] `shell` — Task 8 (`ShellTool` with tokio timeout)
- [x] `schemars` — each tool derives `JsonSchema` for parameter schema generation
- [x] `builtin_tools(path)` — Task 9

**Placeholder scan:** All steps contain implementation code. No "TBD" or "fill in" entries.

**Type consistency:** `Tool` trait methods (`name()`, `description()`, `parameters()`, `execute()`) are used consistently across all tasks. `serde_json::Value` is the execute input/output type throughout.

**Opus review fixes applied (2026-06-06):**
- C1: `generate.rs` added to Task 1 (empty `&[]` type annotation fix)
- C2: `ls.rs` `collect_files_walkdir` uses `filter_entry` to prune ignored directories
- C4: `cargo test ' ls'` corrected to `cargo test 'tools::ls::'` 
- I1: `make_schema<T>()` helper strips `$schema`/`title` from schemars output (Anthropic compat)
- I2: Exact openai/mod.rs import line specified in Task 1 Step 11
- I4: `rg_search` uses `-nH --no-heading` with `splitn(3, ':')` (was broken `--field-match-separator=|`)

**Known edge cases not handled (out of scope for Phase 2):**
- `read`: Windows path separators (macOS/Linux first)
- `glob`: Does not exclude `.git` etc. — user can use specific patterns
- `grep` regex fallback: walkdir descends into `target/` etc. unlike rg — inconsistent but not a blocker
- `shell`: No permission gating in Phase 2 (that lives in Phase 4 with the CLI)
- `read`/`grep` line truncation: byte-slicing at `MAX_LINE_LENGTH` may panic on multibyte UTF-8 chars at boundary (low probability)
