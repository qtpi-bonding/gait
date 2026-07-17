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
        anyhow::bail!("patch must start with *** Begin Patch and end with *** End Patch")
    }
    let mut headers = Vec::new();
    for (i, line) in lines.iter().enumerate().skip(1).take(lines.len() - 2) {
        if line.starts_with("*** Add File: ") || line.starts_with("*** Update File: ") {
            headers.push((i, *line));
        } else if line.starts_with("*** Delete File:")
            || line.starts_with("*** Move to:")
            || line.starts_with("*** End of File")
        {
            anyhow::bail!("unsupported patch operation: {line}")
        }
    }
    if headers.len() != 1 {
        anyhow::bail!("patch must contain exactly one Add or Update file header")
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
            if body.iter().any(|l| !l.starts_with('+')) {
                anyhow::bail!("Add File body must contain only + lines")
            }
            Ok(Patch {
                operation,
                path,
                hunks: vec![Hunk {
                    lines: body
                        .iter()
                        .map(|l| HunkLine::Insert(l[1..].to_string()))
                        .collect(),
                }],
            })
        }
        Operation::Update => {
            if body.is_empty() {
                anyhow::bail!("Update File requires at least one hunk")
            }
            let mut hunks = Vec::new();
            let mut current: Option<Vec<HunkLine>> = None;
            for line in body {
                if line.starts_with("@@") {
                    if let Some(lines) = current.take() {
                        if lines.is_empty() {
                            anyhow::bail!("empty hunk")
                        }
                        hunks.push(Hunk { lines });
                    }
                    current = Some(Vec::new());
                } else {
                    let h = current
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("hunk content before @@"))?;
                    let parsed = match line.as_bytes().first() {
                        Some(b' ') => HunkLine::Context(line[1..].to_string()),
                        Some(b'-') => HunkLine::Delete(line[1..].to_string()),
                        Some(b'+') => HunkLine::Insert(line[1..].to_string()),
                        _ => anyhow::bail!("malformed hunk line: {line}"),
                    };
                    h.push(parsed);
                }
            }
            if let Some(lines) = current {
                if lines.is_empty() {
                    anyhow::bail!("empty hunk")
                }
                hunks.push(Hunk { lines });
            }
            if hunks.is_empty() {
                anyhow::bail!("Update File requires a hunk")
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
        anyhow::bail!("patch target must be a non-empty relative path")
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
            for hunk in &patch.hunks {
                let before: Vec<String> = hunk
                    .lines
                    .iter()
                    .filter_map(|l| match l {
                        HunkLine::Context(s) | HunkLine::Delete(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                if before.is_empty() {
                    anyhow::bail!("hunk has no context or deletion")
                }
                let matches: Vec<usize> = (cursor..=result.len().saturating_sub(before.len()))
                    .filter(|&i| result[i..i + before.len()] == before[..])
                    .collect();
                if matches.len() != 1 {
                    anyhow::bail!(
                        "hunk context is {}",
                        if matches.is_empty() {
                            "missing"
                        } else {
                            "ambiguous"
                        }
                    )
                }
                let at = matches[0];
                let replacement: Vec<String> = hunk
                    .lines
                    .iter()
                    .filter_map(|l| match l {
                        HunkLine::Context(s) | HunkLine::Insert(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                result.splice(at..at + before.len(), replacement);
                cursor = at;
            }
            Ok(result.join("\n"))
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct ApplyPatchParams {
    /// Codex-compatible patch body for one relative Add or Update target.
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
        "Apply one Codex-compatible Add or Update patch to one relative file. Use multiple calls for multiple files."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    async fn execute(&self, args: Value, _rec: &dyn crate::types::Rec) -> Result<Value, ToolError> {
        let params: ApplyPatchParams =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let patch = parse(&params.patch).map_err(ToolError::Internal)?;
        let target =
            resolve_target(&self.ctx.codebase_path, &patch).map_err(ToolError::Internal)?;
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
        let new = apply(&patch, old.as_deref()).map_err(ToolError::Internal)?;
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
}
