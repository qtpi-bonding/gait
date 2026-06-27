use crate::tools::ToolCtx;
use crate::tools::ToolError;
use crate::types::{ExecutionMode, Tool};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Deserialize, JsonSchema)]
struct GlobParams {
    /// The glob pattern to match files against (e.g. '**/*.ts', 'src/**/*.json')
    pattern: String,
    /// Directory to search in (relative to codebase root). Omit to search entire codebase.
    path: Option<String>,
}
#[allow(dead_code)]
#[derive(schemars::JsonSchema)]
struct GlobOutput {
    paths: Vec<String>,
}

pub struct GlobTool {
    ctx: ToolCtx,
    parameters: Value,
    output_schema: Value,
}

impl GlobTool {
    pub fn new(ctx: ToolCtx) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<GlobParams>(),
            crate::types::OutputFilter::Both,
        );
        Self {
            ctx,
            parameters,
            output_schema: crate::tools::make_schema::<GlobOutput>(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files in the codebase matching a glob pattern. Results sorted by modification time (newest first)."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&self.output_schema)
    }

    fn output_filter(&self) -> crate::types::OutputFilter {
        crate::types::OutputFilter::Parse
    }

    async fn execute(&self, args: Value, _rec: &crate::Recorder) -> Result<Value, ToolError> {
        let params: GlobParams = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::InvalidParams(e.to_string()))?;

        let search_dir = match &params.path {
            Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.ctx.codebase_path.join(p),
            None => self.ctx.codebase_path.clone(),
        };

        if !search_dir.exists() {
            return Err(crate::tools::ToolError::Execute(format!(
                "Directory not found: {}",
                search_dir.display()
            )));
        }

        let pattern = params.pattern.clone();

        // Run the filesystem walk on a blocking thread so the async reactor
        // stays alive during the scan (large repos can take many seconds).
        // This also ensures the spine can transition to HandsPhase before the
        // stall-cap watchdog fires.
        let files = tokio::task::spawn_blocking(
            move || -> Result<Vec<(PathBuf, Option<std::time::SystemTime>)>, String> {
                // Compile the glob pattern for per-path matching.
                // We match against the path relative to search_dir so that
                // patterns like `**/*.rs` and `src/foo.dart` work correctly.
                let pat = ::glob::Pattern::new(&pattern)
                    .map_err(|e| format!("Invalid glob pattern: {e}"))?;

                let mut files = Vec::new();

                // ignore::WalkBuilder respects .gitignore, .git/info/exclude,
                // and global gitignore — so build-artifact trees (.cargo-target,
                // build/, .dart_tool/) are pruned automatically.
                for entry in ignore::WalkBuilder::new(&search_dir)
                    .hidden(false) // include dot-files (e.g. .env) unless gitignored
                    .build()
                {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    // Match against the relative path so the pattern doesn't
                    // need to know the absolute search root.
                    let rel = match path.strip_prefix(&search_dir) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    if pat.matches_path(rel) {
                        let mtime = path.metadata().ok().and_then(|m| m.modified().ok());
                        files.push((path.to_path_buf(), mtime));
                    }
                }

                Ok(files)
            },
        )
        .await
        .map_err(|e| crate::tools::ToolError::Execute(format!("Glob task panicked: {e}")))?
        .map_err(crate::tools::ToolError::Execute)?;

        if files.is_empty() {
            return Ok(Value::String("No files found".into()));
        }

        let mut files = files;
        files.sort_by(|a, b| b.1.cmp(&a.1));

        let lines: Vec<String> = files.iter().map(|(p, _)| p.display().to_string()).collect();

        Ok(Value::String(lines.join("\n")))
    }

    fn access(&self) -> crate::types::ToolAccess {
        crate::types::ToolAccess {
            reads: true,
            ..Default::default()
        }
    }

    fn scope_targets(&self, args: &serde_json::Value) -> Vec<(PathBuf, crate::types::Dimension)> {
        // `path` constrains the search root. No explicit path = codebase root (in ceiling).
        // `pattern` is a glob pattern, not a concrete path — not checked here.
        let resolved = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.ctx.codebase_path.join(p),
            None => return vec![],
        };
        vec![(resolved, crate::types::Dimension::Read)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCtx;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_ctx(dir: &TempDir) -> ToolCtx {
        ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "test".into(),
        )
    }

    #[tokio::test]
    async fn finds_files_by_extension() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.ts"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();

        let tool = GlobTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "*.rs" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
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

        let tool = GlobTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "**/*.rs" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("main.rs"), "got: {text}");
        assert!(!text.contains("README.md"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_no_files_found_when_empty() {
        let dir = TempDir::new().unwrap();
        let tool = GlobTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "*.xyz" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        assert_eq!(result.as_str().unwrap(), "No files found");
    }

    #[tokio::test]
    async fn searches_in_subdir_when_path_given() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/deep.rs"), "").unwrap();
        std::fs::write(dir.path().join("root.rs"), "").unwrap();

        let tool = GlobTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "*.rs",
                    "path": "sub"
                }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("deep.rs"), "got: {text}");
        assert!(!text.contains("root.rs"), "got: {text}");
    }

    #[test]
    fn schema_has_parse_array_field() {
        let tool = GlobTool::new(ToolCtx::new(
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
            "test".into(),
        ));
        let params = tool.parameters();
        assert_eq!(
            params["properties"]["parse"]["type"].as_str().unwrap(),
            "array",
            "parse should be array type: {}",
            params
        );
    }
}
