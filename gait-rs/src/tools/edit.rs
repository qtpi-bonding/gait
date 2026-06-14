use std::path::PathBuf;
use crate::hashline::{format_hash_line, apply_hashline_edits, HashlineEditOp, parse_line_ref};
use crate::types::{Tool, ToolPermission, PermissionRequest};
use crate::permission::path_patterns;
use crate::tools::ToolCtx;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// Compute the approximate region [start, end] (1-indexed, inclusive) that `op`
/// produces in the new file.  Exact for the bottom-most op in a multi-edit call
/// (since bottom-up application leaves its position unchanged); slightly off for
/// ops higher in the file if other ops shifted lines, but good enough for a ±N
/// context snippet.
fn op_new_region(op: &HashlineEditOp) -> Option<(usize, usize)> {
    match op.op.as_str() {
        "replace" => {
            let start = parse_line_ref(op.pos.as_deref()?).ok()?.line;
            let count = op.lines.len().max(1);
            Some((start, start + count - 1))
        }
        "append" => {
            let pos = parse_line_ref(op.pos.as_deref()?).ok()?.line;
            let count = op.lines.len().max(1);
            Some((pos + 1, pos + count))
        }
        "prepend" => {
            let pos = parse_line_ref(op.pos.as_deref()?).ok()?.line;
            let count = op.lines.len().max(1);
            Some((pos, pos + count - 1))
        }
        _ => None,
    }
}

/// Format a focused post-edit snippet (changed region ± `context` lines) with
/// fresh hashline annotations — analogous to Claude Code's `cat -n` snippet.
/// The model sees the new state around each change; if a delimiter was dropped
/// it is absent from the snippet and immediately visible.
fn format_edit_snippet(new: &str, ops: &[HashlineEditOp], context: usize) -> String {
    let new_lines: Vec<&str> = if new.is_empty() { vec![] } else { new.split('\n').collect() };
    let n = new_lines.len();

    // Union all op regions to find the overall changed window.
    let mut region_lo: Option<usize> = None;
    let mut region_hi: Option<usize> = None;
    for op in ops {
        if let Some((s, e)) = op_new_region(op) {
            region_lo = Some(region_lo.map_or(s, |lo: usize| lo.min(s)));
            region_hi = Some(region_hi.map_or(e, |hi: usize| hi.max(e)));
        }
    }

    let mut out = String::new();

    if let (Some(lo), Some(hi)) = (region_lo, region_hi) {
        let lo = lo.saturating_sub(context).max(1);
        let hi = (hi + context).min(n);

        if lo > 1 { out.push_str("    ...\n"); }
        for i in lo..=hi {
            if let Some(&line) = new_lines.get(i - 1) {
                out.push_str(&format_hash_line(i, line));
                out.push('\n');
            }
        }
        if hi < n { out.push_str("    ...\n"); }
    }

    let trimmed = out.trim_end().to_string();
    if trimmed.is_empty() { "(no visible changes)".to_string() } else { trimmed }
}

#[derive(Deserialize, JsonSchema)]
struct HashlineEditParams {
    /// Path to the file (relative to codebase root, or absolute)
    file_path: String,
    /// List of edit operations. Each references lines by their N#XY anchors from the most
    /// recent `read` of this file. Ops: "replace" (swap lines), "append" (insert after),
    /// "prepend" (insert before). All refs are validated before any write.
    edits: Vec<HashlineEditOp>,
}

/// Resolve `file_path`, read the file, and apply the hashline edits.
/// Returns `(resolved_path, old_content, new_content)`.
/// Errors on a missing file or a stale/invalid anchor (apply failure).
fn compute_edit(
    ctx: &ToolCtx,
    params: &HashlineEditParams,
) -> anyhow::Result<(PathBuf, String, String)> {
    let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
        PathBuf::from(&params.file_path)
    } else {
        ctx.codebase_path.join(&params.file_path)
    };
    if !resolved.exists() {
        anyhow::bail!("File not found: {}", resolved.display());
    }
    let old_content = std::fs::read_to_string(&resolved)?;
    let new_content = apply_hashline_edits(&old_content, &params.edits)?;
    Ok((resolved, old_content, new_content))
}

pub struct EditTool {
    ctx: ToolCtx,
    parameters: Value,
}

impl EditTool {
    pub fn new(ctx: ToolCtx) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<HashlineEditParams>(),
            crate::types::OutputFilter::Both,
        );
        Self { ctx, parameters }
    }
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str { "edit" }

    fn description(&self) -> &str {
        "Edit a file using hash-anchored line references from `read` output. \
        Always `read` the file first — the N#XY prefix on each line is the anchor for this tool. \
        Pass one or more edits; all refs are validated before any write, so a stale anchor \
        (line changed since your read) returns a clear error with updated refs instead of silently \
        corrupting the file. \
        Operations: \"replace\" (swap a line or range N#XY..M#XY), \
        \"append\" (insert after a line), \"prepend\" (insert before). \
        To replace MORE THAN ONE line you MUST set `end` to the last line's ref — a `replace` \
        with only `pos` swaps exactly that single line, so any extra lines you supply land below \
        the original ones (duplication). Put ONLY the new content in `lines`; never repeat the \
        anchored line or surrounding context. To add code without removing anything, use `append`. \
        Returns the changed region (±2 context lines) with fresh anchors. \
        Re-read the file if you need refs outside the returned region."
    }

    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value, _rec: &crate::Recorder) -> anyhow::Result<Value> {
        let params: HashlineEditParams = serde_json::from_value(args)?;
        let (resolved, old_content, new_content) = compute_edit(&self.ctx, &params)?;
        std::fs::write(&resolved, &new_content)?;

        let lines_changed = (new_content.split('\n').count() as i64
            - old_content.split('\n').count() as i64)
            .unsigned_abs() as usize;

        let snippet = format_edit_snippet(&new_content, &params.edits, 2);
        Ok(Value::String(format!(
            "Edited {} ({} line(s) net change).\n{}",
            resolved.display(),
            lines_changed,
            snippet
        )))
    }

    fn permission(&self, args: &Value) -> ToolPermission {
        let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
        let rel = std::path::Path::new(raw)
            .strip_prefix(&*self.ctx.codebase_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: rel.clone(),
            patterns: path_patterns("edit", &rel),
        }])
    }

    fn preview(&self, args: &Value) -> Option<String> {
        serde_json::from_value::<HashlineEditParams>(args.clone())
            .ok()
            .and_then(|params| compute_edit(&self.ctx, &params).ok())
            .and_then(|(_, old, new)| crate::tools::preview::diff_preview(&old, &new))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::compute_line_hash;
    use tempfile::TempDir;
    use std::path::PathBuf;
    use crate::tools::ToolCtx;

    fn make_ctx(dir: &TempDir) -> ToolCtx {
        ToolCtx::new(dir.path().to_path_buf(), dir.path().to_path_buf(), "test".into())
    }

    // Regression for the weak-model (MiniMax) `edit` failures: the schema must be
    // self-contained (no $ref/definitions, which weak OpenAI-compatible models
    // mis-handle), and `lines` must tolerate the {item:[...]} array-wrapping those
    // models emit instead of erroring "invalid type: map, expected a sequence".
    #[test]
    fn edit_schema_is_inlined_no_ref() {
        let s = crate::tools::make_schema::<HashlineEditParams>();
        let txt = serde_json::to_string(&s).unwrap();
        assert!(!txt.contains("$ref"), "edit schema must be inlined (no $ref) for weak-model compatibility: {txt}");
    }

    #[test]
    fn lines_accepts_minimax_wrapped_array() {
        let payload = serde_json::json!({
            "file_path": "f.rs",
            "edits": [{"op":"replace","pos":"1#AB","lines":{"item":["x","y"]}}]
        });
        let parsed: HashlineEditParams = serde_json::from_value(payload)
            .expect("wrapped {item:[...]} lines must parse");
        assert_eq!(parsed.edits[0].lines, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn lines_accepts_plain_array_and_bare_string() {
        let p1: HashlineEditParams = serde_json::from_value(serde_json::json!({
            "file_path":"f","edits":[{"op":"append","lines":["a","b"]}]
        })).unwrap();
        assert_eq!(p1.edits[0].lines, vec!["a".to_string(), "b".to_string()]);
        let p2: HashlineEditParams = serde_json::from_value(serde_json::json!({
            "file_path":"f","edits":[{"op":"append","lines":"a\nb"}]
        })).unwrap();
        assert_eq!(p2.edits[0].lines, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn replaces_single_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn foo() {\n    bar();\n}").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.split('\n').collect();
        let hash = compute_line_hash(2, lines[1]);

        let tool = EditTool::new(make_ctx(&dir));
        let result = tool.execute(serde_json::json!({
            "file_path": "f.rs",
            "edits": [{"op": "replace", "pos": format!("2#{}", hash), "lines": ["    baz();"]}]
        }), &crate::Recorder::detached()).await.unwrap();

        assert!(result.as_str().unwrap().contains("baz"), "result: {result}");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("baz()"));
        assert!(!on_disk.contains("bar()"));
    }

    // End-to-end regression for the MiniMax duplication snowball (session s60ec2202): the model
    // sends the whole replacement block as ONE array element with embedded \n, omits `end`, and
    // repeats the anchored line as the first line. Pre-fix this quadrupled a match arm; now it
    // must be rejected with guidance rather than silently duplicating.
    #[tokio::test]
    async fn rejects_minimax_snowball_shape() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("chat.rs");
        std::fs::write(&path, "fn run() {\n    match line {\n        _ => {}\n    }\n}").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.split('\n').collect();
        let hash = compute_line_hash(2, lines[1]); // anchor "    match line {"

        let tool = EditTool::new(make_ctx(&dir));
        let result = tool.execute(serde_json::json!({
            "file_path": "chat.rs",
            "edits": [{
                "op": "replace",
                "pos": format!("2#{}", hash),
                // single element, embedded newlines, repeats the anchor line — the snowball shape
                "lines": ["    match line {\n        \"/u\" => {}\n        _ => {}\n    }"]
            }]
        }), &crate::Recorder::detached()).await;

        assert!(result.is_err(), "snowball-shaped edit must be rejected, got: {result:?}");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("end"), "error should steer toward `end`: {msg}");
        // File must be untouched (no partial/duplicated write).
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }

    #[tokio::test]
    async fn returns_error_on_stale_ref() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.rs"), "fn foo() {}").unwrap();

        let tool = EditTool::new(make_ctx(&dir));
        let result = tool.execute(serde_json::json!({
            "file_path": "f.rs",
            "edits": [{"op": "replace", "pos": "1#ZZ", "lines": ["fn bar() {}"]}]
        }), &crate::Recorder::detached()).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(">>>"));
    }

    // Response is a ±2-line context snippet (not the full file) with fresh hashline refs.
    #[tokio::test]
    async fn returns_snippet_not_full_file() {
        let dir = TempDir::new().unwrap();
        // 10-line file; edit line 5
        let content = (1..=10).map(|i| format!("line{}", i)).collect::<Vec<_>>().join("\n");
        std::fs::write(dir.path().join("f.txt"), &content).unwrap();
        let lines: Vec<&str> = content.split('\n').collect();
        let hash = compute_line_hash(5, lines[4]);

        let tool = EditTool::new(make_ctx(&dir));
        let result = tool.execute(serde_json::json!({
            "file_path": "f.txt",
            "edits": [{"op": "replace", "pos": format!("5#{}", hash), "lines": ["REPLACED"]}]
        }), &crate::Recorder::detached()).await.unwrap();

        let text = result.as_str().unwrap();
        // Must show the changed line and ±2 context
        assert!(text.contains("|REPLACED"), "should show new content: {text}");
        assert!(text.contains("|line3"), "should show 2 lines before: {text}");
        assert!(text.contains("|line7"), "should show 2 lines after: {text}");
        // Must NOT show lines far from the change
        assert!(!text.contains("|line1"), "should not show lines outside ±2 context: {text}");
        assert!(!text.contains("|line10"), "should not show lines outside ±2 context: {text}");
        // Should contain fresh hashline refs
        assert!(text.contains('#'), "should contain hash refs: {text}");
    }

    // Range-replace: snippet shows the new state; if a brace was dropped it is absent.
    #[tokio::test]
    async fn range_replace_shows_result_snippet() {
        let dir = TempDir::new().unwrap();
        let content = "fn foo() {\n    let x = 1;\n    let y = 2;\n    x + y\n}";
        std::fs::write(dir.path().join("f.rs"), content).unwrap();
        let lines: Vec<&str> = content.split('\n').collect();
        let h2 = compute_line_hash(2, lines[1]);
        let h4 = compute_line_hash(4, lines[3]);

        let tool = EditTool::new(make_ctx(&dir));
        let result = tool.execute(serde_json::json!({
            "file_path": "f.rs",
            "edits": [{"op": "replace", "pos": format!("2#{}", h2), "end": format!("4#{}", h4), "lines": ["    let z = 3;"]}]
        }), &crate::Recorder::detached()).await.unwrap();

        let text = result.as_str().unwrap();
        assert!(text.contains("let z = 3"), "should show replacement: {text}");
        assert!(text.contains('#'), "should have fresh refs: {text}");
        assert!(!text.contains("let x = 1"), "removed lines should not appear: {text}");
    }


    #[test]
    fn preview_returns_diff_for_valid_edit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn foo() {\n    bar();\n}").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.split('\n').collect();
        let hash = compute_line_hash(2, lines[1]);

        let tool = EditTool::new(make_ctx(&dir));
        let args = serde_json::json!({
            "file_path": "f.rs",
            "edits": [{"op": "replace", "pos": format!("2#{}", hash), "lines": ["    baz();"]}]
        });
        let preview = tool.preview(&args).expect("preview should be Some");
        assert!(preview.contains("-    bar();"), "preview:\n{preview}");
        assert!(preview.contains("+    baz();"), "preview:\n{preview}");
    }

    #[test]
    fn preview_is_none_for_stale_anchor() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn foo() {\n    bar();\n}").unwrap();
        let tool = EditTool::new(make_ctx(&dir));
        let args = serde_json::json!({
            "file_path": "f.rs",
            "edits": [{"op": "replace", "pos": "2#ZZ", "lines": ["    baz();"]}]
        });
        assert_eq!(tool.preview(&args), None);
    }

    #[tokio::test]
    async fn returns_error_on_stale_ref_via_snippet_path() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello\nworld").unwrap();
        let content = "hello\nworld";
        let lines: Vec<&str> = content.split('\n').collect();
        let hash = compute_line_hash(1, lines[0]);

        let tool = EditTool::new(make_ctx(&dir));
        let result = tool.execute(serde_json::json!({
            "file_path": "f.txt",
            "edits": [{"op": "replace", "pos": format!("1#{}", hash), "lines": ["goodbye"]}]
        }), &crate::Recorder::detached()).await.unwrap();

        let text = result.as_str().unwrap();
        assert!(text.contains('#'), "should contain hash refs: {text}");
        assert!(text.contains("|goodbye"), "should show new content: {text}");
    }
}
