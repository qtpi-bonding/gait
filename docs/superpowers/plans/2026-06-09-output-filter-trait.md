# OutputFilter Trait Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Status: DONE** (verified 2026-06-10 — all 10 tasks complete)

**Goal:** Lift `parse` (structured rg+regex pipeline) and `curate_prompt` (LLM annotation filter) from `ShellTool`/`CurateTool` onto the `Tool` trait as an `OutputFilter` enum, so any tool can opt in; remove `CurateTool` entirely.

**Architecture:** Add `OutputFilter { None, Parse, Curate }` to `types.rs` and `fn output_filter()` to the `Tool` trait (default `None`). A helper `inject_filter()` in `tools/mod.rs` decorates a tool's JSON schema with the appropriate optional field. `Parse` injects a structured `parse` field — an array of `{"rg": "<pattern>"}` and/or `{"sd": ["<from>", "<to>"]}` operations applied in sequence using the `grep-regex`/`grep-searcher` crates (rg step) and `regex::bytes::Regex::replace_all` (sd step, already a transitive dep) — **no subprocess, no shell injection surface, linear-time regex with no backtracking**. `Curate` injects `curate_prompt` routed through an annotate subagent. Each opt-in tool overrides `output_filter()` and calls `inject_filter()` in `new()`. `turn.rs` extracts the filter field from args after `execute_tool_call()` returns (in-memory only — transcript keeps raw result, consistent with `post_tool` hooks). `CurateTool` is deleted.

**Tech Stack:** Rust, tokio, serde_json, async-trait, `grep-regex` + `grep-searcher` (ripgrep library crates), `regex` (already a transitive dep — used for the `sd` find/replace step)

---

## Files

**Modified:**
- `gait-rs/Cargo.toml` — add `grep-regex`, `grep-searcher` (no new `sd` dep — use `regex::bytes` already in tree)
- `gait-rs/src/types.rs` — add `OutputFilter` enum + `fn output_filter()` on `Tool` trait
- `gait-rs/src/tools/mod.rs` — add `inject_filter()` helper; remove `curate` mod+use
- `gait-rs/src/tools/shell.rs` — remove internal `parse`; override `output_filter` → `Parse`
- `gait-rs/src/tools/read.rs` — override `output_filter` → `Parse`
- `gait-rs/src/tools/glob.rs` — override `output_filter` → `Parse`
- `gait-rs/src/tools/grep.rs` — override `output_filter` → `Parse`
- `gait-rs/src/tools/ls.rs` — override `output_filter` → `Parse`
- `gait-rs/src/tools/webfetch.rs` — override `output_filter` → `Curate`
- `gait-rs/src/tools/codesearch.rs` — override `output_filter` → `Curate`
- `gait-rs/src/turn.rs` — add `apply_parse_filter` + `apply_curate_filter`; wire both into parallel and serial execution loops
- `gait-rs/src/bin/gait.rs` — remove `CurateTool` push

**Deleted:**
- `gait-rs/src/tools/curate.rs`

**Unchanged:**
- `gait-rs/src/tools/annotate.rs` — still used by `apply_curate_filter` in `turn.rs`

---

## Task 1: Add Cargo dependencies

**Files:**
- Modify: `gait-rs/Cargo.toml`

**Note:** `sd` (the CLI tool) is binary-only — it has no `lib.rs` and cannot be used as a library crate. Do **not** add it. The `sd` find/replace operation is implemented with `regex::bytes::Regex` (already a transitive dep via `grep-regex`). We only need two new crates: `grep-regex` and `grep-searcher`.

- [ ] **Step 1: Add the two crates**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo add grep-regex grep-searcher
```

- [ ] **Step 2: Verify `grep-regex` and `grep-searcher` are importable**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 | grep -E "^error" | head -10
```

Expected: clean build.

- [ ] **Step 3: Verify `regex::bytes` is already present (transitive dep)**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo tree | grep "^regex "
```

Expected: `regex v1.x.x` appears (pulled by `grep-regex`).

- [ ] **Step 4: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add Cargo.toml Cargo.lock && git commit -m "chore: add grep-regex, grep-searcher dependencies"
```

---

## Task 2: Add `OutputFilter` to `types.rs` and `Tool` trait

**Files:**
- Modify: `gait-rs/src/types.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` block in `src/types.rs`:

```rust
#[test]
fn tool_def_output_filter_default_is_none() {
    let tool = make_test_tool_def("grep");
    assert_eq!(tool.output_filter(), OutputFilter::None);
}

#[test]
fn output_filter_variants_are_distinct() {
    assert_ne!(OutputFilter::None, OutputFilter::Parse);
    assert_ne!(OutputFilter::None, OutputFilter::Curate);
    assert_ne!(OutputFilter::Parse, OutputFilter::Curate);
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test output_filter 2>&1 | tail -10
```

Expected: FAIL — `OutputFilter` not defined.

- [ ] **Step 3: Add the enum and trait method**

In `src/types.rs`, after the `ExecutionSignal` enum (around line 151):

```rust
/// Whether a tool supports post-execution output filtering.
/// `Parse` injects a `parse` field — an array of rg/sd operations applied in-process.
/// `Curate` injects a `curate_prompt` field routed through an annotation subagent.
/// `None` (default) injects nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFilter {
    None,
    Parse,
    Curate,
}
```

Add to the `Tool` trait (after `execution_signal`, before `boxed`):

```rust
/// Whether this tool's output can be post-processed before the model sees it.
/// Default: None. Override to opt into parse or curate filtering.
fn output_filter(&self) -> OutputFilter {
    OutputFilter::None
}
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test output_filter 2>&1 | tail -10
```

Expected: PASS — 2 tests pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/types.rs && git commit -m "feat: add OutputFilter enum + output_filter() to Tool trait"
```

---

## Task 3: Add `inject_filter` helper to `tools/mod.rs`

**Files:**
- Modify: `gait-rs/src/tools/mod.rs`

The `parse` field is an **array** of operation objects. Each item is either `{"rg": "<pattern>"}` (keep only matching lines) or `{"sd": ["<from>", "<to>"]}` (find/replace). Operations are applied in sequence.

- [ ] **Step 1: Write failing tests**

Add to `#[cfg(test)]` in `src/tools/mod.rs`:

```rust
#[test]
fn inject_filter_parse_adds_array_field() {
    use crate::types::OutputFilter;
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "cmd": { "type": "string" } }
    });
    let result = inject_filter(schema, OutputFilter::Parse);
    let parse_field = &result["properties"]["parse"];
    assert!(parse_field.is_object(), "parse field missing: {result}");
    assert_eq!(parse_field["type"].as_str().unwrap(), "array");
}

#[test]
fn inject_filter_curate_adds_curate_prompt_field() {
    use crate::types::OutputFilter;
    let schema = serde_json::json!({ "type": "object", "properties": {} });
    let result = inject_filter(schema, OutputFilter::Curate);
    assert!(result["properties"]["curate_prompt"].is_object());
    assert_eq!(result["properties"]["curate_prompt"]["type"].as_str().unwrap(), "string");
}

#[test]
fn inject_filter_none_is_noop() {
    use crate::types::OutputFilter;
    let schema = serde_json::json!({ "type": "object", "properties": {} });
    let result = inject_filter(schema, OutputFilter::None);
    let props = result["properties"].as_object().unwrap();
    assert!(!props.contains_key("parse"));
    assert!(!props.contains_key("curate_prompt"));
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test inject_filter 2>&1 | tail -10
```

Expected: FAIL.

- [ ] **Step 3: Add `inject_filter`**

In `src/tools/mod.rs`, add after `make_schema`:

```rust
/// Inject an optional filter field into an existing JSON Schema `properties` map.
/// `Parse` adds a structured `parse` array field (rg/sd pipeline, applied in-process).
/// `Curate` adds a `curate_prompt` string field (LLM annotation subagent).
/// `None` returns the schema unchanged.
pub fn inject_filter(mut schema: serde_json::Value, filter: crate::types::OutputFilter) -> serde_json::Value {
    use crate::types::OutputFilter;
    match filter {
        OutputFilter::None => return schema,
        OutputFilter::Parse => {
            if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
                props.insert("parse".into(), serde_json::json!({
                    "type": "array",
                    "description": "Optional pipeline of filter operations applied in-process before \
                                    the result reaches the model. Operations run in order. \
                                    {\"rg\": \"<pattern>\"} keeps only lines matching the regex \
                                    (uses linear-time finite automaton — no backtracking, no ReDoS). \
                                    {\"sd\": [\"<from>\", \"<to>\"]} replaces all regex matches of \
                                    <from> with <to> (multiline mode, anchors work per line).",
                    "items": {
                        "type": "object",
                        "description": "One operation: {\"rg\": \"pattern\"} or {\"sd\": [\"from\", \"to\"]}"
                    }
                }));
            }
        }
        OutputFilter::Curate => {
            if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
                props.insert("curate_prompt".into(), serde_json::json!({
                    "type": "string",
                    "description": "Route this tool's output through a 1-turn annotation subagent \
                                    before the model sees it. Describe what counts as relevant. \
                                    Returns {relevant: [...], irrelevant: [...]}."
                }));
            }
        }
    }
    schema
}
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test inject_filter 2>&1 | tail -10
```

Expected: PASS — 3 tests pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/tools/mod.rs && git commit -m "feat: add inject_filter() helper — parse as rg/sd array, curate as string"
```

---

## Task 4: Add `apply_parse_filter` to `turn.rs`

**Files:**
- Modify: `gait-rs/src/turn.rs`

`apply_parse_filter` runs a pipeline of `rg` (grep) and `sd` (find/replace) operations in-process using the `grep-regex`/`grep-searcher` and `sd` library crates. **No subprocess. No shell injection surface.** The function is synchronous — no async needed.

- [ ] **Step 1: Write failing tests**

Add to `#[cfg(test)]` in `src/turn.rs`:

```rust
#[test]
fn apply_parse_filter_rg_keeps_matching_lines() {
    let raw = "line1\nFAILED: test_foo\nline3\n";
    let ops = serde_json::json!([{"rg": "FAILED"}]);
    let result = apply_parse_filter(raw, &ops);
    assert!(result.contains("FAILED: test_foo"), "got: {result}");
    assert!(!result.contains("line1"), "got: {result}");
    assert!(!result.contains("line3"), "got: {result}");
}

#[test]
fn apply_parse_filter_sd_replaces_text() {
    let raw = "error: something went wrong\nerror: another thing\n";
    let ops = serde_json::json!([{"sd": ["error: ", "ERR: "]}]);
    let result = apply_parse_filter(raw, &ops);
    assert!(result.contains("ERR: something"), "got: {result}");
    assert!(!result.contains("error: "), "got: {result}");
}

#[test]
fn apply_parse_filter_pipeline_rg_then_sd() {
    let raw = "ok\nFAILED: test_foo\nFAILED: test_bar\n";
    let ops = serde_json::json!([{"rg": "FAILED"}, {"sd": ["FAILED: ", ""]}]);
    let result = apply_parse_filter(raw, &ops);
    assert!(result.contains("test_foo"), "got: {result}");
    assert!(result.contains("test_bar"), "got: {result}");
    assert!(!result.contains("ok"), "got: {result}");
    assert!(!result.contains("FAILED: "), "got: {result}");
}

#[test]
fn apply_parse_filter_rg_no_match_returns_empty() {
    let raw = "line1\nline2\n";
    let ops = serde_json::json!([{"rg": "NOTFOUND"}]);
    let result = apply_parse_filter(raw, &ops);
    assert!(result.trim().is_empty() || result.contains("(no matches)"), "got: {result}");
}

#[test]
fn apply_parse_filter_empty_ops_returns_raw() {
    let raw = "hello world";
    let ops = serde_json::json!([]);
    let result = apply_parse_filter(raw, &ops);
    assert_eq!(result, raw);
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test apply_parse_filter 2>&1 | tail -15
```

Expected: FAIL — function not defined.

- [ ] **Step 3: Add `apply_parse_filter`**

Add before `pub async fn run_turn` in `src/turn.rs`. Add these imports at the top of `turn.rs`:

```rust
use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, Sink, SinkMatch};
```

Then the function:

```rust
/// Run a pipeline of rg (grep) and sd-style (find/replace) operations on `raw` in-process.
/// `ops` is a JSON array of {"rg": "<pattern>"} and/or {"sd": ["<from>", "<to>"]} items.
/// No subprocess, no shell injection surface — pure in-process regex, linear time, no backtracking.
fn apply_parse_filter(raw: &str, ops: &serde_json::Value) -> String {
    let ops = match ops.as_array() {
        Some(a) => a,
        None => return raw.to_string(),
    };
    if ops.is_empty() {
        return raw.to_string();
    }

    let mut current = raw.to_string();

    for op in ops {
        if let Some(pattern) = op.get("rg").and_then(|v| v.as_str()) {
            // grep: keep only lines matching pattern
            struct LineCollector(Vec<String>);
            impl Sink for LineCollector {
                type Error = std::io::Error;
                fn matched(
                    &mut self,
                    _searcher: &Searcher,
                    mat: &SinkMatch<'_>,
                ) -> Result<bool, Self::Error> {
                    let line = String::from_utf8_lossy(mat.bytes()).into_owned();
                    self.0.push(line.trim_end_matches('\n').to_string());
                    Ok(true)
                }
            }
            current = match RegexMatcher::new(pattern) {
                Ok(matcher) => {
                    let mut collector = LineCollector(vec![]);
                    let _ = Searcher::new().search_slice(
                        &matcher,
                        current.as_bytes(),
                        &mut collector,
                    );
                    if collector.0.is_empty() {
                        "(no matches)".to_string()
                    } else {
                        collector.0.join("\n")
                    }
                }
                Err(e) => format!("[rg error: invalid pattern {:?}: {}]", pattern, e),
            };
        } else if let Some(sd_args) = op.get("sd").and_then(|v| v.as_array()) {
            // find/replace: use regex::bytes (already in tree as dep of grep-regex).
            // NOTE: do NOT use the `sd` CLI crate — it is binary-only and has no lib API.
            // This is what `sd` does internally: multi-line regex replace on raw bytes.
            let from = sd_args.first().and_then(|v| v.as_str()).unwrap_or("");
            let to   = sd_args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            current = match regex::bytes::RegexBuilder::new(from)
                .multi_line(true)
                .build()
            {
                Ok(re) => {
                    let replaced = re.replace_all(current.as_bytes(), to.as_bytes());
                    String::from_utf8_lossy(&replaced).into_owned()
                }
                Err(e) => format!("[sd error: invalid pattern {:?}: {}]", from, e),
            };
        }
        // unknown op keys are silently skipped
    }

    current
}
```

**Note on `grep-searcher` Sink:** `SinkMatch::bytes()` returns the raw matched line bytes (verified against `grep-searcher` source: `sink.rs:379`). `Searcher::search_slice(&matcher, bytes, &mut sink)` accepts borrowed matcher and sink via blanket impls. The `Sink::matched` signature is `(&mut self, &Searcher, &SinkMatch) -> Result<bool, Self::Error>` where `true` means continue.

**Note on `regex::bytes`:** The `regex` crate is already a transitive dep pulled by `grep-regex`. Use `regex::bytes::RegexBuilder` (not `regex::RegexBuilder`) for byte-string replacement so the output round-trips cleanly through UTF-8 without allocating intermediate strings.

- [ ] **Step 4: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test apply_parse_filter 2>&1 | tail -15
```

Expected: 5 tests PASS.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/turn.rs && git commit -m "feat: add apply_parse_filter (rg+sd in-process, no subprocess)"
```

---

## Task 5: Add `apply_curate_filter` to `turn.rs`

**Files:**
- Modify: `gait-rs/src/turn.rs`

The curate filter runs a 1-iter `run_turn` with only `AnnotateTool`, reads the transcript for the `annotate` call, and returns `{relevant, irrelevant}`.

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)]` in `src/turn.rs`:

```rust
#[tokio::test]
async fn apply_curate_filter_returns_annotate_result() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc-ann".into(),
                    tool_name: "annotate".into(),
                    args: serde_json::json!({
                        "relevant": ["foo bar"],
                        "irrelevant": ["baz"]
                    }),
                },
                StreamChunk::Done,
            ],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let result = apply_curate_filter(
        "foo bar\nbaz\n",
        "keep foo lines",
        ws.path(),
        ws.path(),
        "mock/test",
        provider,
    ).await;

    let relevant = result["relevant"].as_array().unwrap();
    assert_eq!(relevant.len(), 1);
    assert_eq!(relevant[0].as_str().unwrap(), "foo bar");
    let irrelevant = result["irrelevant"].as_array().unwrap();
    assert_eq!(irrelevant.len(), 1);
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test apply_curate_filter 2>&1 | tail -15
```

Expected: FAIL.

- [ ] **Step 3: Add `apply_curate_filter`**

Add after `apply_parse_filter` in `src/turn.rs`:

```rust
const CURATE_SYSTEM_PROMPT: &str =
    "You are a curator. Call annotate() exactly once with the relevant and irrelevant items. \
     Do not call any other tool. Do not respond with text.";

async fn apply_curate_filter(
    raw: &str,
    curate_prompt: &str,
    workspace: &Path,
    codebase_path: &Path,
    model: &str,
    provider: Arc<dyn Provider>,
) -> serde_json::Value {
    use crate::tools::annotate::AnnotateTool;
    use crate::session::transcript::read_events;

    let session_id = crate::chat::new_session_id();
    let task = format!(
        "Content to curate:\n{}\n\n---\nCuration task: {}",
        raw, curate_prompt
    );

    let _ = run_turn(
        TurnOpts {
            session_id: session_id.clone(),
            task: Some(task),
            auto: true,
            workspace: workspace.to_path_buf(),
            codebase_path: codebase_path.to_path_buf(),
            allow: vec![],
            deny: vec![],
            system_prompt: CURATE_SYSTEM_PROMPT.to_string(),
            model: model.to_string(),
            mcp_registry: vec![],
            mcp_clients: HashMap::new(),
            skill_registry: vec![],
            config_enabled: std::collections::HashSet::new(),
            max_iter: Some(1),
            hooks: vec![],
        },
        provider,
        vec![AnnotateTool::new().boxed()],
    ).await;

    let tx_path = workspace.join("sessions").join(&session_id).join("transcript.jsonl");
    read_events(&tx_path)
        .ok()
        .and_then(|events| {
            events.into_iter().rev().find_map(|e| {
                if let AnyEvent::ToolCall { name, input, .. } = e {
                    if name == "annotate" { Some(input) } else { None }
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| serde_json::json!({"relevant": [], "irrelevant": []}))
}
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test apply_curate_filter 2>&1 | tail -15
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/turn.rs && git commit -m "feat: add apply_curate_filter to turn.rs"
```

---

## Task 6: Wire filters into `turn.rs` execution loops

**Files:**
- Modify: `gait-rs/src/turn.rs`

Filters are applied in-memory after `execute_tool_call` returns (transcript retains raw result, same as `post_tool` hooks). Three wiring sites:
1. Parallel batch loop (after `join_all`)
2. Serial `Continue` branch
3. Serial `LoadContext` branch

- [ ] **Step 1: Write the failing integration test**

Add to `#[cfg(test)]` in `src/turn.rs`:

```rust
#[tokio::test]
async fn parse_filter_applied_in_memory_transcript_keeps_raw() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc-parse".into(),
                    tool_name: "noisy_tool".into(),
                    args: serde_json::json!({
                        "parse": [{"rg": "FAIL"}]
                    }),
                },
                StreamChunk::Done,
            ],
            vec![
                StreamChunk::Text("done".into()),
                StreamChunk::Done,
            ],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    struct NoisyTool;
    #[async_trait::async_trait]
    impl Tool for NoisyTool {
        fn name(&self) -> &str { "noisy_tool" }
        fn description(&self) -> &str { "test" }
        fn parameters(&self) -> &serde_json::Value {
            static P: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            P.get_or_init(|| serde_json::json!({"type":"object","properties":{}}))
        }
        fn output_filter(&self) -> crate::types::OutputFilter { crate::types::OutputFilter::Parse }
        fn permission(&self, _: &serde_json::Value) -> crate::types::ToolPermission {
            crate::types::ToolPermission::Passthrough
        }
        async fn execute(&self, _: serde_json::Value) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::Value::String("ok_line\nFAIL: bad thing\nok_line2".into()))
        }
    }

    run_turn(TurnOpts {
        session_id: "sfilter01".into(),
        task: Some("run it".into()),
        auto: true,
        workspace: ws.path().to_path_buf(),
        codebase_path: ws.path().to_path_buf(),
        allow: vec![], deny: vec![], system_prompt: "".into(), model: "mock/test".into(),
        mcp_registry: vec![], mcp_clients: std::collections::HashMap::new(),
        skill_registry: vec![], config_enabled: std::collections::HashSet::new(),
        max_iter: None, hooks: vec![],
    }, provider, vec![Box::new(NoisyTool)]).await.unwrap();

    // Transcript keeps RAW result
    let tx_path = ws.path().join("sessions/sfilter01/transcript.jsonl");
    let events = read_events(&tx_path).unwrap();
    let raw_result = events.iter().find_map(|e| {
        if let AnyEvent::ToolResult { id, output, .. } = e {
            if id == "tc-parse" { Some(output.clone()) } else { None }
        } else { None }
    }).expect("should have ToolResult");
    assert!(raw_result.contains("ok_line"), "transcript must have raw: {raw_result}");
    assert!(raw_result.contains("FAIL: bad thing"), "transcript must have raw: {raw_result}");
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test parse_filter_applied 2>&1 | tail -20
```

Expected: FAIL.

- [ ] **Step 3: Add `OutputFilter` to the `use crate::types::` import at top of `turn.rs`**

```rust
use crate::types::{Message, MessageContent, AssistantContent, AssistantPart, ToolCall, ToolResult,
    ToolResultKind, Usage, ExecutionMode, ExecutionSignal, ToolPermission, Hook, HookAction,
    OutputFilter};
```

- [ ] **Step 4: Wire filter into the parallel results loop**

Find the `for ((i, tc), raw) in parallel_items.iter().zip(par_raw)` loop (around line 697). After the `post_tool` hooks block, add:

```rust
// Apply output filter — in-memory only; transcript keeps raw (same as post_tool hooks).
let result_str = result.result.as_str().map(|s| s.to_string())
    .unwrap_or_else(|| result.result.to_string());
if let Some(parse_ops) = tc.args.get("parse") {
    result.result = serde_json::Value::String(apply_parse_filter(&result_str, parse_ops));
} else if let Some(curate_prompt) = tc.args.get("curate_prompt").and_then(|v| v.as_str()) {
    result.result = apply_curate_filter(
        &result_str, curate_prompt,
        &opts.workspace, &opts.codebase_path, &opts.model, provider.clone(),
    ).await;
}
```

- [ ] **Step 5: Wire filter into the serial `Continue` branch**

Find `ExecutionSignal::Continue` (around line 828). After its `post_tool` hooks block, add the same block as Step 4.

- [ ] **Step 6: Wire filter into the serial `LoadContext` branch**

Find `ExecutionSignal::LoadContext` (around line 742). After its `post_tool` hooks block, add the same block.

- [ ] **Step 7: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

Expected: all pass.

- [ ] **Step 8: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/turn.rs && git commit -m "feat: wire parse/curate output filters in turn.rs execution loops"
```

---

## Task 7: Update `shell.rs` — remove internal parse, opt into trait

**Files:**
- Modify: `gait-rs/src/tools/shell.rs`

Shell's internal `parse: Option<String>` (shell pipeline) is removed; the tool now opts into the trait-level `parse` (rg+sd array). The tmp file path is still included in the result — it's written by shell's own execute path already (full output always written), so the model can still reference it.

- [ ] **Step 1: Baseline — confirm existing test passes**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test parse_filters_output 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 2: Remove `parse` from `ShellParams`**

```rust
#[derive(Deserialize, JsonSchema)]
struct ShellParams {
    cmd: String,
    reason: String,
    timeout_ms: Option<u64>,
    // parse removed — now handled by OutputFilter::Parse in turn.rs
}
```

- [ ] **Step 3: Remove internal parse handling from `execute()`**

Replace the `let display = if let Some(parse_cmd) = params.parse { ... } else { ... }` block with:

```rust
let display = format!("{}\n{}", combined.trim_end(), file_note);
```

- [ ] **Step 4: Override `output_filter` and update `new()`**

In `impl Tool for ShellTool`, add:
```rust
fn output_filter(&self) -> crate::types::OutputFilter {
    crate::types::OutputFilter::Parse
}
```

In `ShellTool::new()`:
```rust
let parameters = crate::tools::inject_filter(
    crate::tools::make_schema::<ShellParams>(),
    crate::types::OutputFilter::Parse,
);
```

- [ ] **Step 5: Update the test**

Replace the old `parse_filters_output` test (tests internal subprocess pipe that no longer exists) with:

```rust
#[test]
fn schema_has_parse_array_field() {
    let tool = ShellTool::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
    );
    let params = tool.parameters();
    assert_eq!(params["properties"]["parse"]["type"].as_str().unwrap(), "array",
        "parse should be array type: {params}");
}
```

- [ ] **Step 6: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

Expected: all pass.

- [ ] **Step 7: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/tools/shell.rs && git commit -m "refactor: remove shell internal parse; use OutputFilter::Parse trait"
```

---

## Task 8: Opt `read`, `glob`, `grep`, `ls` into `Parse`

**Files:**
- Modify: `gait-rs/src/tools/read.rs`, `glob.rs`, `grep.rs`, `ls.rs`

- [ ] **Step 1: Write failing schema tests (one per file)**

Each file gets:
```rust
#[test]
fn schema_has_parse_array_field() {
    // use appropriate constructor for each tool
    let tool = ReadTool::new(std::path::PathBuf::from("/tmp"));
    assert_eq!(
        tool.parameters()["properties"]["parse"]["type"].as_str().unwrap(),
        "array"
    );
}
```

Repeat for `GlobTool::new(PathBuf::from("/tmp"))`, `GrepTool::new(PathBuf::from("/tmp"))`, `LsTool::new(PathBuf::from("/tmp"))`.

- [ ] **Step 2: Run to verify all 4 fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test "schema_has_parse_array" 2>&1 | tail -15
```

Expected: 4 FAIL.

- [ ] **Step 3: Update each tool**

For each of `read.rs`, `glob.rs`, `grep.rs`, `ls.rs`:

**Add to `impl Tool for <Name>Tool`:**
```rust
fn output_filter(&self) -> crate::types::OutputFilter {
    crate::types::OutputFilter::Parse
}
```

**Update `new()`** — replace `make_schema::<XParams>()` with:
```rust
crate::tools::inject_filter(
    crate::tools::make_schema::<XParams>(),
    crate::types::OutputFilter::Parse,
)
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/tools/read.rs src/tools/glob.rs src/tools/grep.rs src/tools/ls.rs && git commit -m "feat: opt read/glob/grep/ls into OutputFilter::Parse"
```

---

## Task 9: Opt `webfetch` and `codesearch` into `Curate`

**Files:**
- Modify: `gait-rs/src/tools/webfetch.rs`, `gait-rs/src/tools/codesearch.rs`

- [ ] **Step 1: Write failing schema tests**

```rust
// webfetch.rs
#[test]
fn schema_has_curate_prompt_field() {
    let tool = WebfetchTool::new();
    assert_eq!(
        tool.parameters()["properties"]["curate_prompt"]["type"].as_str().unwrap(),
        "string"
    );
}

// codesearch.rs
#[test]
fn schema_has_curate_prompt_field() {
    let tool = CodesearchTool::new(std::path::PathBuf::from("/tmp"));
    assert_eq!(
        tool.parameters()["properties"]["curate_prompt"]["type"].as_str().unwrap(),
        "string"
    );
}
```

- [ ] **Step 2: Run to verify both fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test "schema_has_curate_prompt" 2>&1 | tail -10
```

Expected: 2 FAIL.

- [ ] **Step 3: Update both tools**

For each — add `output_filter()` override and update `new()` with `inject_filter(..., OutputFilter::Curate)`. Same pattern as Task 8.

- [ ] **Step 4: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/tools/webfetch.rs src/tools/codesearch.rs && git commit -m "feat: opt webfetch/codesearch into OutputFilter::Curate"
```

---

## Task 10: Remove `CurateTool`

**Files:**
- Delete: `gait-rs/src/tools/curate.rs`
- Modify: `gait-rs/src/tools/mod.rs`, `gait-rs/src/bin/gait.rs`

- [ ] **Step 1: Delete `curate.rs`**

```bash
rm /Users/aicoder/Documents/ideation/gait/gait-rs/src/tools/curate.rs
```

- [ ] **Step 2: Remove from `mod.rs`**

Remove `pub mod curate;` and `pub use curate::CurateTool;`.

- [ ] **Step 3: Remove from `gait.rs`**

Remove:
```rust
tools.push(gait_ai::tools::CurateTool::new(
    provider.clone(), workspace.clone(), codebase_path.clone(), model.clone(),
).boxed());
```

- [ ] **Step 4: Build and test**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 | grep "^error" | head -10
cargo test 2>&1 | tail -20
```

Expected: clean build, all tests pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add -u && git commit -m "refactor: remove CurateTool — curate/parse now live on Tool trait"
```

---

## Self-Review

**Spec coverage:**
1. `OutputFilter { None, Parse, Curate }` + trait method — Task 2 ✓
2. `inject_filter` with array schema for `parse` — Task 3 ✓
3. `apply_parse_filter` using `grep-regex`/`grep-searcher` + `sd` (no subprocess) — Task 4 ✓
4. `apply_curate_filter` 1-iter annotate subagent — Task 5 ✓
5. Filter wired in parallel + serial loops — Task 6 ✓
6. `shell.rs` internal parse removed — Task 7 ✓
7. `read/glob/grep/ls` opt into Parse — Task 8 ✓
8. `webfetch/codesearch` opt into Curate — Task 9 ✓
9. CurateTool deleted — Task 10 ✓

**Security (addresses Opus review risk):**
`parse` no longer spawns `sh -c` with model-controlled content. Operations are pure in-process Rust:
- `rg`: `grep-regex`/`grep-searcher` — finite-automaton regex, linear time, no backtracking, no ReDoS (size limits enforced at compile time), reads bytes, no filesystem/network access
- `sd`: `regex::bytes::RegexBuilder` — same `regex` crate guarantees; pure text transform, no subprocess
Neither can execute shell commands. The attack surface of read-only tools (`read`, `glob`, `grep`, `ls`) is unchanged.

**Note: `sd` crate is binary-only** — `cargo add sd` appears to succeed but adds a dep with no lib target. Any `use sd::...` is a compile error. The plan uses `regex::bytes` directly, which is what `sd` itself uses internally.

**Known limitations (add TODO comments in `turn.rs`):**
- Resume path (`PermissionsReply` branch) does not apply output filters — acceptable, see original Opus review
- `apply_curate_filter` creates a full git session worktree per call — acceptable for now; add `no_persist` to `TurnOpts` if this becomes a burden
- `awk`-style field selection / arithmetic not supported — add `awk-rs` crate later if needed
- Resume path (`PermissionsReply` branch in `turn.rs`) does not apply output filters — tool calls that were permission-gated and then approved on resume reach the model unfiltered; acceptable for now, document with a `// TODO: apply_parse/curate_filter` comment at the resume site

---

## Implementation Status (verified 2026-06-10)

**Overall: DONE — all 10 tasks complete.**

- [x] **Task 1: Cargo dependencies** — `grep-regex = "0.1.14"` and `grep-searcher = "0.1.16"` present in `gait-rs/Cargo.toml` lines 32–33.

- [x] **Task 2: `OutputFilter` enum + `Tool::output_filter()`** — `types.rs:153–159`: `pub enum OutputFilter { None, Parse, Curate }` with `#[derive(Debug, Clone, Copy, PartialEq, Eq)]`. `Tool` trait method at `types.rs:214–218`: `fn output_filter(&self) -> OutputFilter { OutputFilter::None }`. Tests present at `types.rs:374–377`.

- [x] **Task 3: `inject_filter` helper in `tools/mod.rs`** — `mod.rs:62–99`: `pub fn inject_filter(mut schema, filter) -> Value`. `Parse` branch injects `parse` array field; `Curate` branch injects `curate_prompt` string field; `None` is a no-op. Three unit tests at `mod.rs:185–214`.

- [x] **Task 4: `apply_parse_filter` in `turn.rs`** — `turn.rs:285`: `fn apply_parse_filter(raw: &str, ops: &serde_json::Value) -> String`. Uses `grep_regex::RegexMatcher` + `grep_searcher::{Searcher, Sink, SinkMatch}` for `rg` ops and `regex::bytes::RegexBuilder` for `sd` ops. Five unit tests at `turn.rs:2091–2137`.

- [x] **Task 5: `apply_curate_filter` in `turn.rs`** — `turn.rs:349`: `async fn apply_curate_filter(raw, curate_prompt, workspace, codebase_path, model, provider) -> serde_json::Value`. Runs 1-iter `run_turn` with only `AnnotateTool`, reads transcript for annotate call result.

- [x] **Task 6: Filters wired into all three execution loops** — Three wiring sites confirmed:
  - Parallel batch loop: `turn.rs:905–921`
  - Serial `LoadContext` branch: `turn.rs:995–1011`
  - Serial `Continue` branch: `turn.rs:1078–1094`
  - All sites apply `parse` or `curate_prompt` filter in-memory after `post_tool` hooks; transcript keeps raw result as specified.

- [x] **Task 7: `shell.rs` — internal parse removed, opts into `OutputFilter::Parse`** — `shell.rs:29–31`: `inject_filter(..., OutputFilter::Parse)` in `new()`. `shell.rs:54–55`: `fn output_filter() -> OutputFilter::Parse`. No `parse: Option<String>` in params struct; internal `sh -c` pipe logic gone.

- [x] **Task 8: `read`, `glob`, `grep`, `ls` opt into `Parse`** — All four confirmed:
  - `read.rs:62–64`, `85–86`
  - `glob.rs:25–27`, `42–43`
  - `grep.rs:35–37`, `124–125`
  - `ls.rs:32–34`, `169–170`

- [x] **Task 9: `webfetch`, `codesearch` opt into `Curate`** — Both confirmed:
  - `webfetch.rs:38–40`, `156–157`; schema test at `webfetch.rs:334–337`
  - `codesearch.rs:27–29`, `70–71`; schema test at `codesearch.rs:254–257`

- [x] **Task 10: `CurateTool` removed** — `gait-rs/src/tools/curate.rs` does not exist. No `pub mod curate` or `pub use curate::CurateTool` in `mod.rs`. No `CurateTool` push in `gait-rs/src/bin/gait.rs` (grep returns 0 matches).

**Nothing remaining.** The `PermissionsReply` resume path intentionally does not apply filters (noted as acceptable in plan's Known Limitations).
