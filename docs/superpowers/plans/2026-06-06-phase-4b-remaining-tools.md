# Phase 4b: Remaining Tools Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port `output`, `multiedit`, `codesearch`, and `webfetch` tools to Rust, plus wire up clean output-signal detection in the turn loop (no global mutable state).

**Architecture:** Each tool is a self-contained `src/tools/<name>.rs` file implementing the `Tool` trait. The `output` tool is stateless — it returns `"recorded"` from `execute()`; the turn loop extracts `TurnOutput` directly from `tc.args` when `tc.tool_name == "output"`, via a `let mut output_signal: Option<TurnOutput>` local in the loop. `multiedit` reuses `edit::replace()`. `codesearch` tries `rg` first, falls back to `walkdir` + `regex` (using `filter_entry` for proper directory pruning). `webfetch` uses `reqwest` (already in Cargo.toml) and `regex` (already in Cargo.toml) for char-boundary-safe HTML conversion.

**Tech Stack:** Rust, `tokio`, `reqwest` (already in Cargo.toml), `walkdir` (already in Cargo.toml), `regex` (already in Cargo.toml), `schemars`, `serde_json`

---

## File Map

| Action | File | Purpose |
|--------|------|---------|
| Create | `gait-rs/src/tools/output.rs` | OutputTool — stateless, returns "recorded" |
| Create | `gait-rs/src/tools/multiedit.rs` | MultieditTool — sequential edits via edit::replace() |
| Create | `gait-rs/src/tools/codesearch.rs` | CodesearchTool — ripgrep fallback to walkdir |
| Create | `gait-rs/src/tools/webfetch.rs` | WebfetchTool — HTTP fetch, regex-based HTML conversion |
| Modify | `gait-rs/src/tools/mod.rs` | Register modules incrementally (one per task), final in Task 6 |
| Modify | `gait-rs/src/turn.rs` | output_signal extraction + permission exemption |

---

### Task 1: `output` Tool

**Files:**
- Create: `gait-rs/src/tools/output.rs`
- Modify: `gait-rs/src/tools/mod.rs` (add output only)

- [ ] **Step 1: Write the failing test**

Create `gait-rs/src/tools/output.rs`:

```rust
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct OutputParams {
    /// One sentence summary of what was accomplished this turn
    summary: String,
    /// Structured result for workflow nodes
    result: Option<Value>,
}

pub struct OutputTool {
    parameters: Value,
}

impl OutputTool {
    pub fn new() -> Self {
        let parameters = crate::tools::make_schema::<OutputParams>();
        Self { parameters }
    }
    pub fn boxed(self) -> Box<dyn Tool> { Box::new(self) }
}

#[async_trait::async_trait]
impl Tool for OutputTool {
    fn name(&self) -> &str { "output" }
    fn description(&self) -> &str {
        "Signal that your turn is complete. Always call this as your final action with a one-sentence summary of what you accomplished."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, _args: Value) -> anyhow::Result<Value> {
        Ok(Value::String("recorded".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_returns_recorded() {
        let tool = OutputTool::new();
        let result = tool.execute(serde_json::json!({
            "summary": "added health endpoint"
        })).await.unwrap();
        assert_eq!(result, Value::String("recorded".into()));
    }

    #[tokio::test]
    async fn execute_with_result_still_returns_recorded() {
        let tool = OutputTool::new();
        let result = tool.execute(serde_json::json!({
            "summary": "done",
            "result": { "files_changed": ["a.rs"] }
        })).await.unwrap();
        assert_eq!(result, Value::String("recorded".into()));
    }

    #[test]
    fn name_is_output() {
        let tool = OutputTool::new();
        assert_eq!(tool.name(), "output");
    }

    #[test]
    fn parameters_schema_has_summary_field() {
        let tool = OutputTool::new();
        let schema = tool.parameters();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("summary"), "schema missing summary field");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test output 2>&1 | tail -10
```

Expected: compile error — module not registered yet.

- [ ] **Step 3: Register output in mod.rs**

Add these two lines to `gait-rs/src/tools/mod.rs` (after the existing `pub mod shell;` line):

```rust
pub mod output;
pub use output::OutputTool;
```

Also add to `builtin_tools()` after `ShellTool::new(codebase_path).boxed(),`:

```rust
OutputTool::new().boxed(),
```

The existing 7-tool list becomes 8 tools. Do NOT add `multiedit`, `codesearch`, or `webfetch` — those files don't exist yet and will break compilation.

- [ ] **Step 4: Run test to verify it passes**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test output 2>&1 | tail -10
```

Expected: 4 output tests passing.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port && git add gait-rs/src/tools/output.rs gait-rs/src/tools/mod.rs && git commit -m "feat(tools): add output tool (stateless, returns recorded)"
```

---

### Task 2: Turn Loop — Output Signal Detection

**Files:**
- Modify: `gait-rs/src/turn.rs`

The turn loop currently synthesizes `TurnOutput.summary` from the assistant's plain text response. When the model calls `output(summary="...", result={...})`, we should use those args instead. We also exempt `output` from permission gating so it's never blocked.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `gait-rs/src/turn.rs` inside the `tests` module, after the existing tests:

```rust
    #[tokio::test]
    async fn output_tool_result_used_in_turn_complete() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                // Step 1: model calls output tool
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "output".into(),
                        args: serde_json::json!({ "summary": "fixed the bug", "result": { "files": 2 } }),
                    },
                    StreamChunk::Done,
                ],
                // Step 2: model sends empty text → turn complete
                vec![
                    StreamChunk::Text("".into()),
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let output_tool = ToolDef {
            name: "output".into(),
            description: "signal done".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("recorded".into()))
            })),
        };

        run_turn(
            TurnOpts {
                session_id: "stest_out".into(),
                task: Some("fix a bug".into()),
                auto: true,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
            },
            provider,
            vec![Box::new(output_tool)],
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/stest_out/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();
        let tc_event = events.iter().find(|e| matches!(e, AnyEvent::TurnComplete { .. })).unwrap();
        if let AnyEvent::TurnComplete { output, .. } = tc_event {
            assert_eq!(output.summary, "fixed the bug");
            assert!(output.result.is_some());
        } else {
            panic!("expected TurnComplete");
        }
    }

    #[tokio::test]
    async fn output_tool_exempt_from_permission_gating() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        // non-auto, allow list is empty — output tool should still execute (never gated)
        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "output".into(),
                        args: serde_json::json!({ "summary": "done" }),
                    },
                    StreamChunk::Done,
                ],
                vec![
                    StreamChunk::Text("".into()),
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let output_tool = ToolDef {
            name: "output".into(),
            description: "signal done".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("recorded".into()))
            })),
        };

        run_turn(
            TurnOpts {
                session_id: "stest_out2".into(),
                task: Some("do it".into()),
                auto: false,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
            },
            provider,
            vec![Box::new(output_tool)],
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/stest_out2/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();
        let has_tc = events.iter().any(|e| matches!(e, AnyEvent::TurnComplete { .. }));
        let has_permissions = events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. }));
        assert!(has_tc, "output tool should complete the turn, not pause for permissions");
        assert!(!has_permissions, "output tool should not trigger permission gating");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test output_tool 2>&1 | tail -10
```

Expected: test failures (behavior not yet changed).

- [ ] **Step 3: Modify turn.rs — three changes**

**a) Add `output_signal` before the main `for _step in 0..50` loop** (after `let mut total_usage = Usage::default();`):

```rust
let mut output_signal: Option<TurnOutput> = None;
```

**b) Exempt `output` from permission gating** — change the allowed check:
```rust
// Before:
let allowed = opts.auto || opts.allow.contains(&tc.tool_name);
// After:
let allowed = opts.auto || opts.allow.contains(&tc.tool_name) || tc.tool_name == "output";
```

**c) Capture signal after tool execution** — immediately after this block:
```rust
let output = match tools.iter().find(|t| t.name() == tc.tool_name) {
    Some(tool) => match tool.execute(tc.args.clone()).await {
        Ok(v) => if v.is_string() { v.as_str().unwrap_or("").to_string() } else { v.to_string() },
        Err(e) => format!("error: {}", e),
    },
    None => format!("error: unknown tool '{}'", tc.tool_name),
};
```

Add:
```rust
if tc.tool_name == "output" {
    if let Some(summary) = tc.args.get("summary").and_then(|v| v.as_str()) {
        output_signal = Some(TurnOutput {
            summary: summary.to_string(),
            result: tc.args.get("result").cloned(),
        });
    }
}
```

**d) Use captured signal in `TurnComplete`** — change:
```rust
output: TurnOutput {
    summary: step_text.lines().next().unwrap_or("").to_string(),
    result: None,
},
```
to:
```rust
output: output_signal.take().unwrap_or_else(|| TurnOutput {
    summary: step_text.lines().next().unwrap_or("").to_string(),
    result: None,
}),
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test 2>&1 | tail -10
```

Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port && git add gait-rs/src/turn.rs && git commit -m "feat(turn): output signal extraction + permission exemption"
```

---

### Task 3: `multiedit` Tool

**Files:**
- Create: `gait-rs/src/tools/multiedit.rs`
- Modify: `gait-rs/src/tools/mod.rs` (add multiedit)

- [ ] **Step 1: Create `gait-rs/src/tools/multiedit.rs`**

```rust
use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use crate::tools::edit::replace;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct EditOp {
    /// The text to replace
    old_string: String,
    /// The replacement text
    new_string: String,
    /// Replace all occurrences (default false)
    replace_all: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct MultieditParams {
    /// Path to the file to modify (relative to codebase root, or absolute)
    file_path: String,
    /// Array of edit operations to perform sequentially
    edits: Vec<EditOp>,
}

pub struct MultieditTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl MultieditTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<MultieditParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
    pub fn boxed(self) -> Box<dyn Tool> { Box::new(self) }
}

#[async_trait::async_trait]
impl Tool for MultieditTool {
    fn name(&self) -> &str { "multiedit" }
    fn description(&self) -> &str {
        "Apply multiple sequential edits to a single file. Each edit replaces old_string with new_string. Edits are applied in order."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: MultieditParams = serde_json::from_value(args)?;
        if params.edits.is_empty() {
            anyhow::bail!("edits array must not be empty");
        }

        let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
            PathBuf::from(&params.file_path)
        } else {
            self.codebase_path.join(&params.file_path)
        };

        if !resolved.exists() {
            anyhow::bail!("File not found: {}", resolved.display());
        }
        if resolved.is_dir() {
            anyhow::bail!("Path is a directory, not a file: {}", resolved.display());
        }

        let mut content = std::fs::read_to_string(&resolved)?;

        for (i, edit) in params.edits.iter().enumerate() {
            content = replace(&content, &edit.old_string, &edit.new_string,
                edit.replace_all.unwrap_or(false))
                .map_err(|e| anyhow::anyhow!("Edit {}: {}", i + 1, e))?;
        }

        std::fs::write(&resolved, &content)?;
        Ok(Value::String(format!("Applied {} edits to {}", params.edits.len(), resolved.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn applies_single_edit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "hello world").unwrap();

        let tool = MultieditTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "file.txt",
            "edits": [{ "old_string": "hello", "new_string": "goodbye" }]
        })).await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
    }

    #[tokio::test]
    async fn applies_multiple_edits_in_order() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "alpha beta gamma").unwrap();

        let tool = MultieditTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "file.txt",
            "edits": [
                { "old_string": "alpha", "new_string": "one" },
                { "old_string": "beta", "new_string": "two" },
                { "old_string": "gamma", "new_string": "three" }
            ]
        })).await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one two three");
    }

    #[tokio::test]
    async fn second_edit_can_use_result_of_first() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "foo").unwrap();

        let tool = MultieditTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "file.txt",
            "edits": [
                { "old_string": "foo", "new_string": "foobar" },
                { "old_string": "foobar", "new_string": "baz" }
            ]
        })).await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "baz");
    }

    #[tokio::test]
    async fn error_when_file_not_found() {
        let dir = TempDir::new().unwrap();
        let tool = MultieditTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "nonexistent.txt",
            "edits": [{ "old_string": "a", "new_string": "b" }]
        })).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn error_reports_which_edit_failed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "hello").unwrap();

        let tool = MultieditTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "file.txt",
            "edits": [
                { "old_string": "hello", "new_string": "world" },
                { "old_string": "missing_string", "new_string": "x" }
            ]
        })).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Edit 2"), "expected edit index in error, got: {}", msg);
    }

    #[tokio::test]
    async fn error_on_empty_edits() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "hello").unwrap();

        let tool = MultieditTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "file.txt",
            "edits": []
        })).await;
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: Register multiedit in mod.rs**

Add to `gait-rs/src/tools/mod.rs` after the output lines:

```rust
pub mod multiedit;
pub use multiedit::MultieditTool;
```

Add to `builtin_tools()` after `OutputTool::new().boxed(),`:

```rust
MultieditTool::new(codebase_path.clone()).boxed(),
```

- [ ] **Step 3: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test multiedit 2>&1 | tail -10
```

Expected: all 6 multiedit tests pass.

- [ ] **Step 4: Full suite**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test 2>&1 | tail -10
```

Expected: no regressions.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port && git add gait-rs/src/tools/multiedit.rs gait-rs/src/tools/mod.rs && git commit -m "feat(tools): add multiedit tool"
```

---

### Task 4: `codesearch` Tool

**Files:**
- Create: `gait-rs/src/tools/codesearch.rs`
- Modify: `gait-rs/src/tools/mod.rs` (add codesearch)

Uses `filter_entry` (not `continue`) for proper directory pruning — this prunes the subtree, not just the directory entry itself.

- [ ] **Step 1: Create `gait-rs/src/tools/codesearch.rs`**

```rust
use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const MAX_RESULTS: usize = 50;

#[derive(Deserialize, JsonSchema)]
struct CodesearchParams {
    /// Symbol name, function name, or regex pattern to search for
    query: String,
    /// Directory to search within (relative to codebase root). Defaults to entire codebase.
    path: Option<String>,
    /// Case-sensitive search (default false)
    case_sensitive: Option<bool>,
}

pub struct CodesearchTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl CodesearchTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<CodesearchParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
    pub fn boxed(self) -> Box<dyn Tool> { Box::new(self) }
}

pub fn walk_and_search(dir: &std::path::Path, pattern: &regex::Regex, results: &mut Vec<String>) {
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_str().unwrap_or("");
            !(name.starts_with('.') || name == "node_modules" || name == "target")
        })
        .filter_map(|e| e.ok())
    {
        if results.len() >= MAX_RESULTS { break; }
        if !entry.path().is_file() { continue; }
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (i, line) in content.lines().enumerate() {
            if results.len() >= MAX_RESULTS { break; }
            if pattern.is_match(line) {
                results.push(format!("{}:{}: {}", entry.path().display(), i + 1, line));
            }
        }
    }
}

#[async_trait::async_trait]
impl Tool for CodesearchTool {
    fn name(&self) -> &str { "codesearch" }
    fn description(&self) -> &str {
        "Search for symbols, function names, class definitions, or patterns in the codebase. Use for finding where something is defined or used. Returns file:line matches."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: CodesearchParams = serde_json::from_value(args)?;
        let case_sensitive = params.case_sensitive.unwrap_or(false);

        let search_dir = match &params.path {
            Some(p) => self.codebase_path.join(p),
            None => (*self.codebase_path).clone(),
        };

        // Try ripgrep first (exit 0 = matches, exit 1 = no matches, other = not installed)
        let mut rg_cmd = tokio::process::Command::new("rg");
        rg_cmd.arg("--line-number").arg("--no-heading");
        if !case_sensitive { rg_cmd.arg("-i"); }
        rg_cmd.arg(&params.query).arg(&search_dir);

        if let Ok(out) = rg_cmd.output().await {
            match out.status.code() {
                Some(0) => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
                    if lines.is_empty() {
                        return Ok(Value::String("No matches found.".into()));
                    }
                    let truncated = lines.len() > MAX_RESULTS;
                    let body = lines[..lines.len().min(MAX_RESULTS)].join("\n");
                    let output = if truncated {
                        format!("(first {} results)\n{}", MAX_RESULTS, body)
                    } else {
                        body
                    };
                    return Ok(Value::String(output));
                }
                Some(1) => return Ok(Value::String("No matches found.".into())),
                _ => {} // rg not installed or error — fall through to Rust fallback
            }
        }

        // Fallback: walkdir + regex
        let prefix = if case_sensitive { "" } else { "(?i)" };
        let pattern = match regex::Regex::new(&format!("{}{}", prefix, &params.query)) {
            Ok(p) => p,
            Err(_) => return Ok(Value::String(format!("Invalid search pattern: {}", params.query))),
        };
        let mut results = vec![];
        walk_and_search(&search_dir, &pattern, &mut results);
        if results.is_empty() {
            return Ok(Value::String("No matches found.".into()));
        }
        Ok(Value::String(results.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Direct unit tests for walk_and_search (exercises fallback path independently of rg)

    #[test]
    fn walk_and_search_caps_at_max_results() {
        let dir = TempDir::new().unwrap();
        for i in 0..60 {
            std::fs::write(dir.path().join(format!("f{}.txt", i)), "match_me\n").unwrap();
        }
        let pat = regex::Regex::new("match_me").unwrap();
        let mut results = vec![];
        walk_and_search(dir.path(), &pat, &mut results);
        assert!(results.len() <= MAX_RESULTS, "got {} > {}", results.len(), MAX_RESULTS);
    }

    #[test]
    fn walk_and_search_prunes_dotdirs() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "match_me").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "no_match").unwrap();
        let pat = regex::Regex::new("match_me").unwrap();
        let mut results = vec![];
        walk_and_search(dir.path(), &pat, &mut results);
        assert!(results.is_empty(), "should not search .git, got: {:?}", results);
    }

    #[test]
    fn walk_and_search_prunes_node_modules() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/dep.js"), "match_me").unwrap();
        let pat = regex::Regex::new("match_me").unwrap();
        let mut results = vec![];
        walk_and_search(dir.path(), &pat, &mut results);
        assert!(results.is_empty(), "should not search node_modules, got: {:?}", results);
    }

    // Integration tests via CodesearchTool.execute

    #[tokio::test]
    async fn finds_literal_in_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn hello_world() {\n    println!(\"hi\");\n}").unwrap();

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "hello_world" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("main.rs"), "got: {}", text);
        assert!(text.contains("hello_world"), "got: {}", text);
    }

    #[tokio::test]
    async fn case_insensitive_by_default() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("code.rs"), "struct MyStruct {}").unwrap();

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "mystruct" })).await.unwrap();
        assert!(result.as_str().unwrap().contains("MyStruct"));
    }

    #[tokio::test]
    async fn case_sensitive_does_not_match_wrong_case() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("code.rs"), "struct MyStruct {}").unwrap();

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "mystruct", "case_sensitive": true })).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "No matches found.");
    }

    #[tokio::test]
    async fn no_matches_returns_message() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world").unwrap();

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "totally_nonexistent_xyz" })).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "No matches found.");
    }

    #[tokio::test]
    async fn caps_at_max_results_via_tool() {
        let dir = TempDir::new().unwrap();
        for i in 0..60 {
            std::fs::write(dir.path().join(format!("file_{}.txt", i)), "target_match").unwrap();
        }

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "target_match" })).await.unwrap();
        let text = result.as_str().unwrap();
        let match_lines = text.lines().filter(|l| l.contains("target_match")).count();
        assert!(match_lines <= MAX_RESULTS, "got {} results, expected ≤ {}", match_lines, MAX_RESULTS);
    }

    #[tokio::test]
    async fn invalid_regex_returns_graceful_message() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "test").unwrap();
        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "[invalid_unclosed" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(
            text == "No matches found." || text.contains("Invalid search pattern"),
            "unexpected: {}", text
        );
    }

    #[tokio::test]
    async fn restricts_to_subdirectory() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/inside.txt"), "find_me").unwrap();
        std::fs::write(dir.path().join("outside.txt"), "find_me").unwrap();

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "find_me", "path": "sub" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("inside.txt"), "got: {}", text);
        assert!(!text.contains("outside.txt"), "got: {}", text);
    }
}
```

- [ ] **Step 2: Register codesearch in mod.rs**

Add to `gait-rs/src/tools/mod.rs` after the multiedit lines:

```rust
pub mod codesearch;
pub use codesearch::CodesearchTool;
```

Add to `builtin_tools()` after `MultieditTool::new(codebase_path.clone()).boxed(),`:

```rust
CodesearchTool::new(codebase_path.clone()).boxed(),
```

- [ ] **Step 3: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test codesearch 2>&1 | tail -10
```

Expected: all 10 codesearch tests pass.

- [ ] **Step 4: Full suite**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test 2>&1 | tail -10
```

Expected: no regressions.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port && git add gait-rs/src/tools/codesearch.rs gait-rs/src/tools/mod.rs && git commit -m "feat(tools): add codesearch tool (rg + walkdir fallback, filter_entry pruning)"
```

---

### Task 5: `webfetch` Tool

**Files:**
- Create: `gait-rs/src/tools/webfetch.rs`
- Modify: `gait-rs/src/tools/mod.rs` (add webfetch)

HTML → markdown/text conversion uses the `regex` crate (already in Cargo.toml) for char-boundary-safe processing. The hand-rolled `to_lowercase().find()` approach would panic on non-ASCII input because `to_lowercase()` can change byte length.

- [ ] **Step 1: Create `gait-rs/src/tools/webfetch.rs`**

```rust
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024; // 5MB
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum FetchFormat {
    Text,
    Markdown,
    Html,
}

impl Default for FetchFormat {
    fn default() -> Self { FetchFormat::Markdown }
}

#[derive(Deserialize, JsonSchema)]
struct WebfetchParams {
    /// The URL to fetch content from (must start with http:// or https://)
    url: String,
    /// The format to return content in: text (plain), markdown, or html. Defaults to markdown.
    format: Option<FetchFormat>,
    /// Timeout in seconds (max 120). Defaults to 30.
    timeout: Option<u64>,
}

pub struct WebfetchTool {
    parameters: Value,
}

impl WebfetchTool {
    pub fn new() -> Self {
        let parameters = crate::tools::make_schema::<WebfetchParams>();
        Self { parameters }
    }
    pub fn boxed(self) -> Box<dyn Tool> { Box::new(self) }
}

/// Strip all HTML tags and decode entities. Uses regex for char-boundary-safe processing.
pub fn strip_html(html: &str) -> String {
    // Remove script/style/noscript/iframe block content
    let block = regex::Regex::new(r"(?is)<(script|style|noscript|iframe)[^>]*>.*?</\1>").unwrap();
    let s = block.replace_all(html, "");
    // Convert <br> variants to newline
    let br = regex::Regex::new(r"(?i)<br\s*/?>").unwrap();
    let s = br.replace_all(&s, "\n");
    // Convert closing block elements to newlines (p, h1-h6 → double newline; others → single)
    let p_end = regex::Regex::new(r"(?i)</(p|h[1-6])>").unwrap();
    let s = p_end.replace_all(&s, "\n\n");
    let block_end = regex::Regex::new(r"(?i)</(div|li|tr)>").unwrap();
    let s = block_end.replace_all(&s, "\n");
    // Strip all remaining tags
    let tags = regex::Regex::new(r"<[^>]+>").unwrap();
    let s = tags.replace_all(&s, "");
    // Decode common entities
    let s = s
        .replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
        .replace("&quot;", "\"").replace("&#39;", "'").replace("&nbsp;", " ")
        .replace("&mdash;", "—").replace("&ndash;", "–");
    // Collapse excess whitespace
    let multi_nl = regex::Regex::new(r"\n{3,}").unwrap();
    let s = multi_nl.replace_all(&s, "\n\n");
    let spaces = regex::Regex::new(r"[ \t]+").unwrap();
    let s = spaces.replace_all(&s, " ");
    s.trim().to_string()
}

/// Convert HTML to markdown. Uses regex for char-boundary-safe processing.
pub fn html_to_markdown(html: &str) -> String {
    // Remove script/style blocks
    let block = regex::Regex::new(r"(?is)<(script|style)[^>]*>.*?</\1>").unwrap();
    let s = block.replace_all(html, "").into_owned();

    // Headings h1-h6
    let h = regex::Regex::new(r"(?is)<h([1-6])[^>]*>(.*?)</h[1-6]>").unwrap();
    let s = h.replace_all(&s, |caps: &regex::Captures| {
        let level: usize = caps[1].parse().unwrap_or(1);
        format!("\n{} {}\n", "#".repeat(level), caps[2].trim())
    }).into_owned();

    // Strong/bold
    let strong = regex::Regex::new(r"(?is)<strong[^>]*>(.*?)</strong>").unwrap();
    let s = strong.replace_all(&s, |caps: &regex::Captures| format!("**{}**", &caps[1])).into_owned();
    let b = regex::Regex::new(r"(?is)<b[^>]*>(.*?)</b>").unwrap();
    let s = b.replace_all(&s, |caps: &regex::Captures| format!("**{}**", &caps[1])).into_owned();

    // Em/italic
    let em = regex::Regex::new(r"(?is)<em[^>]*>(.*?)</em>").unwrap();
    let s = em.replace_all(&s, |caps: &regex::Captures| format!("*{}*", &caps[1])).into_owned();
    let i = regex::Regex::new(r"(?is)<i[^>]*>(.*?)</i>").unwrap();
    let s = i.replace_all(&s, |caps: &regex::Captures| format!("*{}*", &caps[1])).into_owned();

    // Code
    let code = regex::Regex::new(r"(?is)<code[^>]*>(.*?)</code>").unwrap();
    let s = code.replace_all(&s, |caps: &regex::Captures| format!("`{}`", &caps[1])).into_owned();

    // Links — capture href and link text
    let a = regex::Regex::new(r#"(?is)<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#).unwrap();
    let s = a.replace_all(&s, |caps: &regex::Captures| {
        format!("[{}]({})", &caps[2], &caps[1])
    }).into_owned();

    // br → newline
    let br = regex::Regex::new(r"(?i)<br\s*/?>").unwrap();
    let s = br.replace_all(&s, "\n").into_owned();

    // li items
    let li = regex::Regex::new(r"(?i)<li[^>]*>").unwrap();
    let s = li.replace_all(&s, "- ").into_owned();

    // Block endings → newlines
    let p_end = regex::Regex::new(r"(?i)</(p|h[1-6])>").unwrap();
    let s = p_end.replace_all(&s, "\n\n").into_owned();
    let block_end = regex::Regex::new(r"(?i)</(div|li|tr)>").unwrap();
    let s = block_end.replace_all(&s, "\n").into_owned();

    // Strip remaining tags
    let tags = regex::Regex::new(r"<[^>]+>").unwrap();
    let s = tags.replace_all(&s, "").into_owned();

    // Entities
    let s = s
        .replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
        .replace("&quot;", "\"").replace("&#39;", "'").replace("&nbsp;", " ");

    // Collapse excessive blank lines
    let multi_nl = regex::Regex::new(r"\n{3,}").unwrap();
    let s = multi_nl.replace_all(&s, "\n\n");
    s.trim().to_string()
}

#[async_trait::async_trait]
impl Tool for WebfetchTool {
    fn name(&self) -> &str { "webfetch" }
    fn description(&self) -> &str {
        "Fetch content from a URL and return it as text or markdown."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: WebfetchParams = serde_json::from_value(args)?;

        if !params.url.starts_with("http://") && !params.url.starts_with("https://") {
            anyhow::bail!("URL must start with http:// or https://");
        }

        let timeout_secs = params.timeout
            .map(|t| t.min(MAX_TIMEOUT_SECS))
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let format = params.format.unwrap_or_default();

        let accept = match format {
            FetchFormat::Markdown => "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1",
            FetchFormat::Text    => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
            FetchFormat::Html    => "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, */*;q=0.1",
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36")
            .build()?;

        let initial = client.get(&params.url)
            .header("Accept", accept)
            .header("Accept-Language", "en-US,en;q=0.9")
            .send().await
            .map_err(|e| anyhow::anyhow!("Request failed: {}", e))?;

        // Retry with honest UA on Cloudflare challenge
        let response = if initial.status() == 403
            && initial.headers().get("cf-mitigated").is_some()
        {
            client.get(&params.url)
                .header("Accept", accept)
                .header("Accept-Language", "en-US,en;q=0.9")
                .header("User-Agent", "gait")
                .send().await
                .map_err(|e| anyhow::anyhow!("Retry request failed: {}", e))?
        } else {
            initial
        };

        if !response.status().is_success() {
            anyhow::bail!("Request failed with status {}: {}",
                response.status().as_u16(),
                response.status().canonical_reason().unwrap_or(""));
        }

        if let Some(len) = response.content_length() {
            if len as usize > MAX_RESPONSE_BYTES {
                anyhow::bail!("Response too large (exceeds 5MB limit)");
            }
        }

        let content_type = response.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let is_html = content_type.contains("text/html");

        let bytes = response.bytes().await?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            anyhow::bail!("Response too large (exceeds 5MB limit)");
        }
        let content = String::from_utf8_lossy(&bytes).into_owned();

        let output = match format {
            FetchFormat::Markdown => if is_html { html_to_markdown(&content) } else { content },
            FetchFormat::Text     => if is_html { strip_html(&content) } else { content },
            FetchFormat::Html     => content,
        };

        Ok(Value::String(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_removes_tags() {
        let html = "<p>Hello <b>world</b>!</p>";
        let result = strip_html(html);
        assert_eq!(result, "Hello world!");
    }

    #[test]
    fn strip_html_removes_script_blocks() {
        let html = "<p>text</p><script>alert('xss')</script><p>more</p>";
        let result = strip_html(html);
        assert!(!result.contains("alert"), "got: {}", result);
        assert!(result.contains("text"), "got: {}", result);
        assert!(result.contains("more"), "got: {}", result);
    }

    #[test]
    fn strip_html_decodes_entities() {
        let html = "<p>a &amp; b &lt; c &gt; d</p>";
        let result = strip_html(html);
        assert!(result.contains("a & b"), "got: {}", result);
        assert!(result.contains("< c"), "got: {}", result);
    }

    #[test]
    fn strip_html_handles_non_ascii() {
        // Non-ASCII content must not panic (char-boundary safety)
        let html = "<p>Héllo wörld — café</p><script>x()</script>";
        let result = strip_html(html);
        assert!(!result.contains("x()"), "script not removed: {}", result);
        assert!(result.contains("Héllo"), "content lost: {}", result);
    }

    #[test]
    fn html_to_markdown_converts_headings() {
        let html = "<h1>Title</h1><h2>Subtitle</h2>";
        let result = html_to_markdown(html);
        assert!(result.contains("# Title"), "got: {}", result);
        assert!(result.contains("## Subtitle"), "got: {}", result);
    }

    #[test]
    fn html_to_markdown_converts_bold_italic() {
        let html = "<strong>bold</strong> and <em>italic</em>";
        let result = html_to_markdown(html);
        assert!(result.contains("**bold**"), "got: {}", result);
        assert!(result.contains("*italic*"), "got: {}", result);
    }

    #[test]
    fn html_to_markdown_converts_links() {
        let html = r#"<a href="https://example.com">click here</a>"#;
        let result = html_to_markdown(html);
        assert!(result.contains("[click here](https://example.com)"), "got: {}", result);
    }

    #[test]
    fn html_to_markdown_converts_code() {
        let html = "<code>let x = 1;</code>";
        let result = html_to_markdown(html);
        assert!(result.contains("`let x = 1;`"), "got: {}", result);
    }

    #[test]
    fn html_to_markdown_handles_non_ascii() {
        // Non-ASCII must not panic
        let html = "<h1>Héllo</h1><p>wörld — café</p>";
        let result = html_to_markdown(html);
        assert!(result.contains("# Héllo"), "got: {}", result);
    }

    #[tokio::test]
    async fn rejects_non_http_url() {
        let tool = WebfetchTool::new();
        let result = tool.execute(serde_json::json!({ "url": "ftp://example.com/file" })).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("http"));
    }

    #[tokio::test]
    async fn rejects_file_url() {
        let tool = WebfetchTool::new();
        let result = tool.execute(serde_json::json!({ "url": "file:///etc/passwd" })).await;
        assert!(result.is_err());
    }

    #[test]
    fn name_is_webfetch() {
        let tool = WebfetchTool::new();
        assert_eq!(tool.name(), "webfetch");
    }
}
```

- [ ] **Step 2: Register webfetch in mod.rs**

Add to `gait-rs/src/tools/mod.rs` after the codesearch lines:

```rust
pub mod webfetch;
pub use webfetch::WebfetchTool;
```

Add to `builtin_tools()` after `CodesearchTool::new(codebase_path.clone()).boxed(),`:

```rust
WebfetchTool::new().boxed(),
```

- [ ] **Step 3: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test webfetch 2>&1 | tail -10
```

Expected: all unit tests pass (URL validation, HTML conversion including non-ASCII, name).

- [ ] **Step 4: Full suite**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test 2>&1 | tail -10
```

Expected: no regressions.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port && git add gait-rs/src/tools/webfetch.rs gait-rs/src/tools/mod.rs && git commit -m "feat(tools): add webfetch tool (regex-based HTML conversion, 5MB limit)"
```

---

### Task 6: Final mod.rs Validation

**Files:**
- Modify: `gait-rs/src/tools/mod.rs` (add count + uniqueness tests)

All 4 tool files now exist. This task adds tests to lock in the `builtin_tools()` count and uniqueness, then runs the full suite for final sign-off.

- [ ] **Step 1: Add tests to mod.rs**

Append to `gait-rs/src/tools/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn builtin_tools_returns_eleven() {
        let tools = builtin_tools(PathBuf::from("/tmp"));
        assert_eq!(tools.len(), 11);
    }

    #[test]
    fn all_tool_names_unique() {
        let tools = builtin_tools(PathBuf::from("/tmp"));
        let mut names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let original_len = names.len();
        names.sort();   // sort before dedup — dedup only removes consecutive duplicates
        names.dedup();
        assert_eq!(names.len(), original_len, "duplicate tool names found");
    }
}
```

- [ ] **Step 2: Run mod tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test builtin 2>&1 | tail -10
```

Expected: both mod tests pass.

- [ ] **Step 3: Full test suite**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo test 2>&1 | tail -20
```

Expected: `test result: ok. N tests; 0 failed` where N ≥ 115.

- [ ] **Step 4: Verify binary builds**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs && cargo build --release 2>&1 | tail -5
```

Expected: `Finished release [optimized]`

- [ ] **Step 5: Final commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port && git add gait-rs/src/tools/mod.rs && git commit -m "test(tools): add builtin_tools count and uniqueness assertions"
```

---

## Issues Fixed From Review

| Issue | Fix Applied |
|-------|-------------|
| C1: mod.rs written with all 11 modules before 3 files exist | Each task registers only its own module incrementally |
| C2: HTML parsing panics on non-ASCII (to_lowercase byte offsets) | Replaced with regex crate (char-boundary-safe, already in Cargo.toml) |
| I1: walk_and_search uses continue, not filter_entry — subtrees not pruned | Changed to filter_entry for proper subtree pruning |
| I2: fallback cap untested when rg is installed | Added direct walk_and_search unit tests (3 tests independent of rg) |
| M3: all_tool_names_unique dedup without sort misses non-adjacent duplicates | Added sort() before dedup() |
