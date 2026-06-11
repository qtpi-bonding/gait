use std::path::PathBuf;
use crate::types::{Tool, ExecutionMode};
use crate::tools::ToolCtx;
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
    ctx: ToolCtx,
    parameters: Value,
}

impl LsTool {
    pub fn new(ctx: ToolCtx) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<LsParams>(),
            crate::types::OutputFilter::Parse,
        );
        Self { ctx, parameters }
    }
}

fn should_ignore(name: &str, extra: &[String]) -> bool {
    IGNORE_NAMES.contains(&name) || extra.iter().any(|e| e == name)
}

fn collect_files_walkdir(dir: &std::path::Path, extra_ignore: &[String]) -> (Vec<PathBuf>, bool) {
    let mut files = vec![];
    let mut truncated = false;
    // filter_entry prunes entire ignored subtrees — `continue` would only skip the entry itself
    // and WalkDir would still descend into node_modules/, target/, etc.
    for entry in walkdir::WalkDir::new(dir).into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !IGNORE_NAMES.contains(&name.as_ref())
        })
        .filter_map(|e| e.ok())
    {
        let name = entry.file_name().to_string_lossy();
        if should_ignore(&name, extra_ignore) { continue; }
        if entry.file_type().is_file() {
            if files.len() >= LS_LIMIT { truncated = true; break; }
            files.push(entry.path().to_path_buf());
        }
    }
    (files, truncated)
}

/// Returns None if rg is unavailable or errored (caller should fall back to walkdir).
/// Returns Some(files, truncated) — possibly empty — when rg ran and produced an authoritative result.
/// rg exit codes: 0 = files found, 1 = no files, 2 = error.
fn collect_files_rg(dir: &std::path::Path, extra_ignore: &[String]) -> anyhow::Result<Option<(Vec<PathBuf>, bool)>> {
    let mut cmd = std::process::Command::new("rg");
    cmd.args(["--files", "--hidden", "--no-messages"]);
    for name in IGNORE_NAMES.iter().copied().chain(extra_ignore.iter().map(|s| s.as_str())) {
        cmd.arg(format!("--glob=!{}/**", name));
    }
    cmd.arg(dir);
    cmd.current_dir(dir);

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None), // rg not installed
        Err(e) => return Err(e.into()),
    };

    match output.status.code() {
        Some(0) | Some(1) => {} // 0 = files found, 1 = no files — both authoritative, don't re-scan
        _ => return Ok(None),   // exit 2 or signal: rg error, fall back to walkdir
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let all: Vec<PathBuf> = text.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect();
    let truncated = all.len() >= LS_LIMIT;
    Ok(Some((all.into_iter().take(LS_LIMIT).collect(), truncated)))
}

fn render_tree(search_dir: &std::path::Path, files: &[PathBuf], truncated: bool) -> String {
    use std::collections::{BTreeMap, BTreeSet};
    let mut dir_files: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut all_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    all_dirs.insert(PathBuf::from("."));

    for file in files {
        let rel = file.strip_prefix(search_dir).unwrap_or(file);
        let parent = rel.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let name = rel.file_name().unwrap_or_default().to_string_lossy().into_owned();
        dir_files.entry(parent.to_path_buf()).or_default().insert(name);
        let mut d = parent.to_path_buf();
        loop {
            all_dirs.insert(d.clone());
            let next = d.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new("."));
            if next == std::path::Path::new(".") || d == std::path::Path::new(".") {
                all_dirs.insert(PathBuf::from("."));
                break;
            }
            d = next.to_path_buf();
        }
    }

    fn render_dir(
        dir_path: &std::path::Path,
        all_dirs: &std::collections::BTreeSet<PathBuf>,
        dir_files: &std::collections::BTreeMap<PathBuf, std::collections::BTreeSet<String>>,
        depth: usize,
    ) -> String {
        let indent = "  ".repeat(depth);
        let mut out = String::new();
        if depth > 0 {
            let name = dir_path.file_name().unwrap_or_default().to_string_lossy();
            out.push_str(&format!("{}{}/\n", indent, name));
        }
        let child_indent = "  ".repeat(depth + 1);
        let children: Vec<&PathBuf> = all_dirs.iter().filter(|d| {
            let par = d.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new("."));
            par == dir_path && *d != dir_path
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
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }

    fn output_filter(&self) -> crate::types::OutputFilter {
        crate::types::OutputFilter::Parse
    }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: LsParams = serde_json::from_value(args)?;
        let extra_ignore = params.ignore.unwrap_or_default();

        let search_dir = match &params.path {
            Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.ctx.codebase_path.join(p),
            None => self.ctx.codebase_path.clone(),
        };

        if !search_dir.exists() {
            anyhow::bail!("Directory not found: {}", search_dir.display());
        }
        if !search_dir.is_dir() {
            anyhow::bail!("Not a directory: {}", search_dir.display());
        }

        let (files, truncated) = match collect_files_rg(&search_dir, &extra_ignore)? {
            Some(r) => r,                                          // rg ran — authoritative (even if empty)
            None => collect_files_walkdir(&search_dir, &extra_ignore), // rg unavailable/errored — fall back
        };

        Ok(Value::String(render_tree(&search_dir, &files, truncated)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::path::PathBuf;
    use crate::tools::ToolCtx;

    fn make_ctx(dir: &TempDir) -> ToolCtx {
        ToolCtx::new(dir.path().to_path_buf(), dir.path().to_path_buf(), "test".into())
    }

    #[tokio::test]
    async fn lists_root_as_tree() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();

        let tool = LsTool::new(make_ctx(&dir));
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

        let tool = LsTool::new(make_ctx(&dir));
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

        let tool = LsTool::new(make_ctx(&dir));
        let result = tool.execute(serde_json::json!({ "path": "sub" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("file.rs"), "got: {text}");
        assert!(!text.contains("root.rs"), "got: {text}");
    }

    #[test]
    fn schema_has_parse_array_field() {
        let tool = LsTool::new(ToolCtx::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"), "test".into()));
        let params = tool.parameters();
        assert_eq!(params["properties"]["parse"]["type"].as_str().unwrap(), "array",
            "parse should be array type: {}", params);
    }
}
