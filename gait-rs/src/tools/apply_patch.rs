//! Independent, ACP-aligned implementation of the supported Codex patch format.
//!
//! The syntax is compatible with OpenAI Codex's Apache-2.0 `apply_patch` format;
//! this parser is authored independently and intentionally supports only one
//! relative Add or Update target per call.

use crate::permissions::path_patterns;
use crate::tools::{ToolCtx, ToolError};
use crate::types::{PermissionRequest, Tool, ToolPermission};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Add,
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub operation: Operation,
    pub path: PathBuf,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The text after `@@`, when it names a line: the hunk's context is
    /// searched for at or after that line.
    pub anchor: Option<String>,
    pub lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkLine {
    Context(String),
    Delete(String),
    Insert(String),
}

pub fn parse(input: &str) -> anyhow::Result<Patch> {
    let lines: Vec<&str> = input.lines().collect();
    if lines.len() < 3 || lines[0] != "*** Begin Patch" || lines.last() != Some(&"*** End Patch") {
        anyhow::bail!(
            "patch must start with a line that is exactly '*** Begin Patch' and end with a \
             line that is exactly '*** End Patch' (got first line {:?}, last line {:?})",
            lines.first().copied().unwrap_or(""),
            lines.last().copied().unwrap_or(""),
        )
    }
    let mut headers = Vec::new();
    for (i, line) in lines.iter().enumerate().skip(1).take(lines.len() - 2) {
        if line.starts_with("*** Add File: ") || line.starts_with("*** Update File: ") {
            headers.push((i, *line));
        } else if line.starts_with("*** Delete File:")
            || line.starts_with("*** Move to:")
            || line.starts_with("*** End of File")
        {
            anyhow::bail!(
                "unsupported patch operation {line:?}: this implementation only supports a \
                 single '*** Add File: <path>' or '*** Update File: <path>' per call — \
                 delete/move/end-of-file markers are not supported"
            )
        }
    }
    if headers.len() != 1 {
        anyhow::bail!(
            "patch must contain exactly one '*** Add File: <path>' or '*** Update File: \
             <path>' header, found {} — use a separate apply_patch call per file",
            headers.len()
        )
    }
    let (header_i, header) = headers[0];
    let (operation, raw_path) = if let Some(p) = header.strip_prefix("*** Add File: ") {
        (Operation::Add, p)
    } else {
        (
            Operation::Update,
            header.strip_prefix("*** Update File: ").unwrap(),
        )
    };
    let path = validate_relative_path(raw_path)?;
    let body = &lines[header_i + 1..lines.len() - 1];
    match operation {
        Operation::Add => {
            if let Some(bad) = body.iter().find(|l| !l.starts_with('+')) {
                anyhow::bail!(
                    "Add File body must contain only '+'-prefixed lines (every line of a \
                     new file is an insertion, with no '@@' header) — found {bad:?}"
                )
            }
            Ok(Patch {
                operation,
                path,
                hunks: vec![Hunk {
                    anchor: None,
                    lines: body
                        .iter()
                        .map(|l| HunkLine::Insert(l[1..].to_string()))
                        .collect(),
                }],
            })
        }
        Operation::Update => {
            if body.is_empty() {
                anyhow::bail!(
                    "Update File requires at least one '@@' hunk header followed by \
                     ' '/'-'/'+'-prefixed lines"
                )
            }
            let mut hunks = Vec::new();
            let mut current: Option<Vec<HunkLine>> = None;
            let mut anchor: Option<String> = None;
            for line in body {
                if let Some(header) = line.strip_prefix("@@") {
                    if let Some(lines) = current.take() {
                        if lines.is_empty() {
                            anyhow::bail!(
                                "empty hunk: a '@@' header was immediately followed by another \
                                 '@@' header (or the end of the patch) with no ' '/'-'/'+' \
                                 lines in between"
                            )
                        }
                        hunks.push(Hunk {
                            anchor: anchor.take(),
                            lines,
                        });
                    }
                    anchor = parse_anchor(header);
                    current = Some(Vec::new());
                } else {
                    let h = current.as_mut().ok_or_else(|| {
                        anyhow::anyhow!(
                            "hunk content before @@: line {line:?} appeared before any \
                             '@@' hunk header — every hunk must start with a line that is \
                             exactly '@@' (or '@@ <context>')"
                        )
                    })?;
                    // A bare empty line (no leading space) is an unchanged blank line in
                    // the file, not a formatting error — models very commonly emit a
                    // truly empty line here instead of remembering the leading space.
                    // The reference Codex parser treats it the same way.
                    let parsed = if line.is_empty() {
                        HunkLine::Context(String::new())
                    } else {
                        match line.as_bytes().first() {
                            Some(b' ') => HunkLine::Context(line[1..].to_string()),
                            Some(b'-') => HunkLine::Delete(line[1..].to_string()),
                            Some(b'+') => HunkLine::Insert(line[1..].to_string()),
                            _ => anyhow::bail!(
                                "malformed hunk line {line:?}: every line inside a hunk must \
                                 start with ' ' (unchanged context), '-' (removed), or '+' \
                                 (added) — or be completely empty for an unchanged blank line"
                            ),
                        }
                    };
                    h.push(parsed);
                }
            }
            if let Some(lines) = current {
                if lines.is_empty() {
                    anyhow::bail!(
                        "empty hunk: the final '@@' header has no ' '/'-'/'+' lines after it"
                    )
                }
                hunks.push(Hunk { anchor, lines });
            }
            if hunks.is_empty() {
                anyhow::bail!(
                    "Update File requires at least one '@@' hunk header followed by \
                     ' '/'-'/'+'-prefixed lines"
                )
            }
            Ok(Patch {
                operation,
                path,
                hunks,
            })
        }
    }
}

pub fn validate_relative_path(raw: &str) -> anyhow::Result<PathBuf> {
    if raw.is_empty() || Path::new(raw).is_absolute() {
        anyhow::bail!(
            "patch target must be a non-empty path RELATIVE to the codebase root (e.g. \
             'src/lib.rs', never '/absolute/path/...' or ''), got {raw:?} — fix the '*** Add \
             File:'/'*** Update File:' header and retry"
        )
    }
    let p = Path::new(raw);
    if p.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        anyhow::bail!("patch target escapes the codebase: {raw}")
    }
    Ok(p.to_path_buf())
}

/// Resolve and verify a target without trusting lexical `join` containment.
pub fn resolve_target(codebase: &Path, patch: &Patch) -> anyhow::Result<PathBuf> {
    let root = codebase
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("codebase is not accessible: {e}"))?;
    let target = root.join(&patch.path);
    let mut cursor = root.clone();
    for component in patch.path.components() {
        cursor.push(component.as_os_str());
        if let Ok(meta) = std::fs::symlink_metadata(&cursor) {
            if meta.file_type().is_symlink() {
                anyhow::bail!("patch target contains a symlink: {}", cursor.display())
            }
        }
    }
    let check = match patch.operation {
        Operation::Update => target
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("update target is not accessible: {e}"))?,
        Operation::Add => {
            let parent = target
                .parent()
                .ok_or_else(|| anyhow::anyhow!("invalid patch target"))?;
            let mut existing = parent;
            while !existing.exists() {
                existing = existing
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("add parent is outside the codebase"))?;
            }
            existing
                .canonicalize()
                .map_err(|e| anyhow::anyhow!("add parent is not accessible: {e}"))?
                .join(target.strip_prefix(existing).unwrap_or(Path::new("")))
        }
    };
    if !check.starts_with(&root) {
        anyhow::bail!("patch target is outside the codebase")
    }
    Ok(target)
}

fn split_lines(s: &str) -> Vec<String> {
    s.split('\n').map(ToString::to_string).collect()
}

pub fn apply(patch: &Patch, old: Option<&str>) -> anyhow::Result<String> {
    match patch.operation {
        Operation::Add => {
            if old.is_some() {
                anyhow::bail!("Add File target already exists")
            }
            Ok(patch.hunks[0]
                .lines
                .iter()
                .map(|l| match l {
                    HunkLine::Insert(s) => s.as_str(),
                    _ => "",
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        Operation::Update => {
            let old = old.ok_or_else(|| anyhow::anyhow!("Update File target does not exist"))?;
            let mut result = split_lines(old);
            let mut cursor = 0;
            for (index, hunk) in patch.hunks.iter().enumerate() {
                let before: Vec<String> = hunk
                    .lines
                    .iter()
                    .filter_map(|l| match l {
                        HunkLine::Context(s) | HunkLine::Delete(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                if before.is_empty() {
                    anyhow::bail!(
                        "hunk has no context or deletion lines: a hunk needs at least one \
                         ' '-prefixed (context) or '-'-prefixed (deletion) line so its \
                         location in the file can be found — a hunk of only '+' lines has \
                         nothing to anchor on"
                    )
                }
                // Codex semantics: an `@@ <line>` anchor moves the search to that
                // line, and every hunk after the first takes the first match after
                // the previous one. Only an unanchored first hunk has nothing to
                // narrow the search, so it alone must match exactly once.
                let mut start = cursor;
                if let Some(anchor) = &hunk.anchor {
                    start = (cursor..result.len())
                        .find(|&i| result[i].trim() == anchor.as_str())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "hunk anchor not found: no line at or after line {} matches \
                                 '@@ {anchor}' (compared with surrounding whitespace \
                                 trimmed); re-read the file and anchor on a line that exists",
                                cursor + 1
                            )
                        })?;
                }
                let matches = find_all(&result, &before, start);
                let must_be_unique = index == 0 && hunk.anchor.is_none();
                let at = match matches.as_slice() {
                    [] => anyhow::bail!(missing_context_error(&result, &before, start)),
                    [only] => *only,
                    [first, ..] if !must_be_unique => *first,
                    many => anyhow::bail!(ambiguous_context_error(&before, many)),
                };
                let replacement: Vec<String> = hunk
                    .lines
                    .iter()
                    .filter_map(|l| match l {
                        HunkLine::Context(s) | HunkLine::Insert(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                cursor = at + replacement.len();
                result.splice(at..at + before.len(), replacement);
            }
            Ok(result.join("\n"))
        }
    }
}

/// `@@ <text>` names an anchor line; a bare `@@` or a unified-diff range
/// header (`@@ -12,5 +12,6 @@`) does not.
fn parse_anchor(header: &str) -> Option<String> {
    let text = header.trim();
    let is_range = text.starts_with('-') && text.ends_with("@@");
    (!text.is_empty() && !is_range).then(|| text.to_string())
}

fn find_all(lines: &[String], needle: &[String], start: usize) -> Vec<usize> {
    if needle.len() > lines.len() {
        return vec![];
    }
    (start..=lines.len() - needle.len())
        .filter(|&i| lines[i..i + needle.len()] == needle[..])
        .collect()
}

fn hunk_preview(before: &[String]) -> String {
    before
        .iter()
        .take(3)
        .map(|l| format!("  {l:?}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn line_list(indices: &[usize]) -> String {
    let shown: Vec<String> = indices
        .iter()
        .take(8)
        .map(|i| (i + 1).to_string())
        .collect();
    let more = if indices.len() > 8 { ", ..." } else { "" };
    format!("{}{more}", shown.join(", "))
}

fn ambiguous_context_error(before: &[String], matches: &[usize]) -> String {
    format!(
        "hunk context is ambiguous: it matches at lines {} — add an '@@ <line>' anchor \
         naming a unique line above the change (for example the enclosing fn or mod \
         line), or add more context lines. First lines of this hunk's context/deletion \
         block:\n{}",
        line_list(matches),
        hunk_preview(before)
    )
}

fn missing_context_error(lines: &[String], before: &[String], start: usize) -> String {
    let first = before[0].trim();
    let near: Vec<usize> = (start..lines.len())
        .filter(|&i| !first.is_empty() && lines[i].trim() == first)
        .collect();
    let hint = match near.as_slice() {
        [] => String::new(),
        [one] => format!(
            " The first context line matches line {} except for whitespace — copy its \
             indentation exactly.",
            one + 1
        ),
        many => format!(
            " The first context line matches lines {} except for whitespace — copy the \
             indentation exactly.",
            line_list(many)
        ),
    };
    format!(
        "hunk context is missing: no location{} matches it (matching is exact — every \
         character, including leading/trailing whitespace and indentation, must match the \
         current file content verbatim; re-read the file and copy the context lines \
         exactly).{hint} First lines of this hunk's context/deletion block:\n{}",
        if start > 0 {
            format!(" at or after line {}", start + 1)
        } else {
            String::new()
        },
        hunk_preview(before)
    )
}

#[derive(Deserialize, JsonSchema)]
struct ApplyPatchParams {
    /// Patch body for one relative Add or Update target.
    patch: String,
}

pub struct ApplyPatchTool {
    ctx: ToolCtx,
    parameters: Value,
}
impl ApplyPatchTool {
    pub fn new(ctx: ToolCtx) -> Self {
        Self {
            ctx,
            parameters: crate::tools::make_schema::<ApplyPatchParams>(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn description(&self) -> &str {
        "Apply an Add or Update patch to one relative file. The patch must use the Begin/End markers. Use one call per file; multiple calls may be made in one response. `@@ <line>` anchors a hunk: its context is searched for at or after that line. Each hunk after the first takes the first match after the previous hunk; an unanchored first hunk must match exactly once."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    async fn execute(&self, args: Value, _rec: &dyn crate::types::Rec) -> Result<Value, ToolError> {
        let params: ApplyPatchParams =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        // Parse, target, and apply failures are all fixable by the model.
        let fixable = |e: anyhow::Error| ToolError::Execute(e.to_string());
        let patch = parse(&params.patch).map_err(fixable)?;
        let target = resolve_target(&self.ctx.codebase_path, &patch).map_err(fixable)?;
        let old = if self.ctx.fs.exists(&target) {
            Some(
                self.ctx
                    .fs
                    .read_to_string(&target)
                    .map_err(|e| ToolError::Internal(e.into()))?,
            )
        } else {
            None
        };
        let new = apply(&patch, old.as_deref()).map_err(fixable)?;
        if let Some(parent) = target.parent() {
            self.ctx
                .fs
                .create_dir_all(parent)
                .map_err(|e| ToolError::Internal(e.into()))?;
        }
        self.ctx
            .fs
            .write(&target, new.as_bytes())
            .map_err(|e| ToolError::Internal(e.into()))?;
        let summary = match patch.operation {
            Operation::Add => format!("Added {}", target.display()),
            Operation::Update => format!("Updated {}", target.display()),
        };
        Ok(Value::String(format!(
            "{summary} ({} bytes)\n{}",
            new.len(),
            crate::tools::preview::diff_preview(old.as_deref().unwrap_or(""), &new)
                .unwrap_or_else(|| "(no changes)".to_string())
        )))
    }
    fn permission(&self, args: &Value) -> ToolPermission {
        let raw = args
            .get("patch")
            .and_then(Value::as_str)
            .and_then(|s| parse(s).ok())
            .map(|p| p.path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "*".into());
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: raw.clone(),
            patterns: path_patterns("apply_patch", &raw),
        }])
    }
    fn preview(&self, args: &Value) -> Option<String> {
        let body = args.get("patch")?.as_str()?;
        let p = parse(body).ok()?;
        let target = resolve_target(&self.ctx.codebase_path, &p).ok()?;
        let old = if self.ctx.fs.exists(&target) {
            self.ctx.fs.read_to_string(&target).ok()?
        } else {
            String::new()
        };
        let new = apply(
            &p,
            if self.ctx.fs.exists(&target) {
                Some(old.as_str())
            } else {
                None
            },
        )
        .ok()?;
        crate::tools::preview::diff_preview(&old, &new).or(Some(new))
    }
    fn access(&self) -> crate::types::ToolAccess {
        crate::types::ToolAccess {
            reads: true,
            writes: true,
            ..Default::default()
        }
    }
    fn scope_targets(&self, args: &Value) -> Vec<(PathBuf, crate::types::Dimension)> {
        let Some(body) = args.get("patch").and_then(Value::as_str) else {
            return vec![];
        };
        let Ok(p) = parse(body) else { return vec![] };
        resolve_target(&self.ctx.codebase_path, &p)
            .ok()
            .map(|p| vec![(p, crate::types::Dimension::Write)])
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_patch_that_does_not_apply_is_a_user_facing_error_not_internal() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("x.txt"), "a\nb\n").unwrap();
        let tool = ApplyPatchTool::new(crate::tools::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "test".into(),
        ));
        let patch = "*** Begin Patch\n*** Update File: x.txt\n@@\n-zzz\n+y\n*** End Patch";
        let err = tool
            .execute(
                serde_json::json!({ "patch": patch }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execute(_)), "{err:?}");
    }
    #[tokio::test]
    async fn a_malformed_patch_is_a_user_facing_error_not_internal() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = ApplyPatchTool::new(crate::tools::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "test".into(),
        ));
        let err = tool
            .execute(
                serde_json::json!({ "patch": "not a patch" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execute(_)), "{err:?}");
    }
    #[test]
    fn add_and_update() {
        let p = parse("*** Begin Patch\n*** Add File: x.txt\n+one\n+two\n*** End Patch").unwrap();
        assert_eq!(apply(&p, None).unwrap(), "one\ntwo");
        let p = parse("*** Begin Patch\n*** Update File: x.txt\n@@\n a\n-b\n+B\n c\n*** End Patch")
            .unwrap();
        assert_eq!(apply(&p, Some("a\nb\nc")).unwrap(), "a\nB\nc");
    }
    #[test]
    fn rejects_bad_paths_and_ambiguous_context() {
        assert!(
            parse("*** Begin Patch\n*** Update File: ../x\n@@\n-a\n+b\n*** End Patch").is_err()
        );
        let p = parse("*** Begin Patch\n*** Update File: x\n@@\n-a\n+b\n*** End Patch").unwrap();
        assert!(apply(&p, Some("a\na")).is_err());
    }
    fn patch(body: &str) -> Patch {
        parse(&format!(
            "*** Begin Patch\n*** Update File: x\n{body}\n*** End Patch"
        ))
        .unwrap()
    }
    #[test]
    fn later_hunk_takes_first_match_after_the_previous_hunk() {
        // The task6 shape: a context-only hunk locates the function, then a
        // one-line `}` context that appears many times in the file.
        let old = "fn a() {\n}\nfn target() {\n    x();\n}\nfn b() {\n}";
        let p = patch("@@\n fn target() {\n@@\n }\n+fn added() {}");
        assert_eq!(
            apply(&p, Some(old)).unwrap(),
            "fn a() {\n}\nfn target() {\n    x();\n}\nfn added() {}\nfn b() {\n}"
        );
    }
    #[test]
    fn anchor_header_narrows_the_search() {
        let old = "mod one {\n    v = 1;\n}\nmod two {\n    v = 1;\n}";
        let p = patch("@@ mod two {\n-    v = 1;\n+    v = 2;");
        assert_eq!(
            apply(&p, Some(old)).unwrap(),
            "mod one {\n    v = 1;\n}\nmod two {\n    v = 2;\n}"
        );
    }
    #[test]
    fn missing_anchor_is_reported() {
        let p = patch("@@ mod three {\n-a\n+b");
        let msg = apply(&p, Some("a")).unwrap_err().to_string();
        assert!(msg.contains("mod three {"), "{msg}");
    }
    #[test]
    fn unanchored_first_hunk_must_be_unique_and_lists_candidates() {
        let old = "a\nx\nb\nx\nc";
        let p = patch("@@\n-x\n+y");
        let msg = apply(&p, Some(old)).unwrap_err().to_string();
        assert!(msg.contains("ambiguous"), "{msg}");
        assert!(
            msg.contains("lines 2, 4"),
            "should list candidate line numbers: {msg}"
        );
        assert!(msg.contains("@@"), "should suggest an anchor: {msg}");
    }
    #[test]
    fn missing_context_points_at_a_whitespace_only_near_miss() {
        let old = "fn f() {\n        let v = 1;\n}";
        let p = patch("@@\n-    let v = 1;\n+    let v = 2;");
        let msg = apply(&p, Some(old)).unwrap_err().to_string();
        assert!(msg.contains("missing"), "{msg}");
        assert!(
            msg.contains("line 2"),
            "should point at the near miss: {msg}"
        );
        assert!(msg.contains("whitespace"), "{msg}");
    }
    #[test]
    fn unified_diff_range_header_is_not_an_anchor() {
        let p = patch("@@ -1,2 +1,2 @@\n a\n-b\n+B");
        assert_eq!(p.hunks[0].anchor, None);
        assert_eq!(apply(&p, Some("a\nb")).unwrap(), "a\nB");
    }
    #[test]
    fn hunks_do_not_rematch_text_an_earlier_hunk_inserted() {
        let old = "x\ny";
        let p = patch("@@\n x\n+y\n@@\n y\n+z");
        assert_eq!(apply(&p, Some(old)).unwrap(), "x\ny\ny\nz");
    }
    #[test]
    fn bare_blank_line_in_hunk_is_unchanged_blank_context() {
        // A model-emitted truly-empty line (no leading space) inside a hunk must be
        // treated as an unchanged blank line, matching the reference Codex parser —
        // not rejected as "malformed hunk line: ".
        let p = parse("*** Begin Patch\n*** Update File: x\n@@\n a\n\n-b\n+B\n c\n*** End Patch")
            .unwrap();
        assert_eq!(apply(&p, Some("a\n\nb\nc")).unwrap(), "a\n\nB\nc");
    }
    #[test]
    fn malformed_hunk_line_error_names_the_offending_line() {
        let err = parse("*** Begin Patch\n*** Update File: x\n@@\n a\n?bogus\n*** End Patch")
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("?bogus"),
            "error should quote the bad line: {msg}"
        );
    }
    #[test]
    fn missing_context_error_previews_the_hunk() {
        let p =
            parse("*** Begin Patch\n*** Update File: x\n@@\n z\n-a\n+b\n*** End Patch").unwrap();
        let err = apply(&p, Some("a\nb\nc")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing"), "error should say missing: {msg}");
        assert!(
            msg.contains("\"z\""),
            "error should preview the context: {msg}"
        );
    }
}
