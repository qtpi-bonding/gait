//! Concrete [`WorkflowRunner`] implementation backed by `gait-workflow`'s
//! `run_workflow`. This is the implementation `gait-repl` and `gait-cli`
//! inject into `ToolCtx` to enable the `run_workflow` builtin tool.

use crate::runner::{run_workflow, WorkflowRunOpts};
use crate::schema::WorkflowDef;
use gait_core::workflow_runner::WorkflowRunner;
use std::path::PathBuf;

/// Concrete [`WorkflowRunner`] that reads workflow yamls from `gait_dir` and
/// spawns each run on a dedicated OS thread with its own tokio runtime (avoids
/// the `!Send` bound on `run_workflow`'s internal futures).
#[derive(Debug)]
pub struct GaitWorkflowRunner {
    pub(crate) gait_dir: PathBuf,
    pub(crate) codebase: PathBuf,
}

impl GaitWorkflowRunner {
    pub fn new(gait_dir: PathBuf, codebase: PathBuf) -> Self {
        Self { gait_dir, codebase }
    }
}

#[async_trait::async_trait]
impl WorkflowRunner for GaitWorkflowRunner {
    async fn run(&self, name: String, args: Vec<String>) -> String {
        let yaml_path = self.gait_dir.join("workflows").join(format!("{name}.yaml"));
        let yaml_content = match std::fs::read_to_string(&yaml_path) {
            Ok(c) => c,
            Err(e) => return format!("workflow error: cannot read {}: {e}", yaml_path.display()),
        };
        let workflow_def: WorkflowDef = match serde_yaml::from_str(&yaml_content) {
            Ok(w) => w,
            Err(e) => {
                return format!(
                    "workflow error: invalid yaml at {}: {e}",
                    yaml_path.display()
                )
            }
        };

        let run_id = format!(
            "{}-{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );

        let yaml_path_str = yaml_path.to_string_lossy().to_string();
        let gait_dir = self.gait_dir.clone();
        let codebase = self.codebase.clone();
        let run_id_task = run_id.clone();
        let run_id_log = run_id.clone();

        // `run_workflow` uses `?Send` futures internally; tokio::spawn requires
        // Send. Use a dedicated OS thread + its own runtime to avoid the bound.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(e) = rt.block_on(run_workflow(WorkflowRunOpts {
                workflow_def,
                yaml_path: yaml_path_str,
                session_id: run_id_task,
                gait_dir,
                codebase,
                args,
            })) {
                eprintln!("[gait workflow] {run_id_log}: error: {e}");
            }
        });

        format!("workflow started: {}", run_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_core::workflow_runner::WorkflowRunner;
    use std::path::PathBuf;

    #[tokio::test]
    async fn run_reports_missing_yaml() {
        let r = GaitWorkflowRunner::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"));
        let msg = r.run("definitely-does-not-exist-xyz".into(), vec![]).await;
        assert!(msg.contains("cannot read"), "got: {msg}");
    }

    #[tokio::test]
    async fn run_reports_empty_name_reads_nonexistent_file() {
        // empty name → constructs path /tmp/workflows/.yaml → cannot read
        let r = GaitWorkflowRunner::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"));
        let msg = r.run(String::new(), vec![]).await;
        // an empty name is allowed by the runner; the yaml just won't exist
        assert!(
            msg.contains("cannot read") || msg.starts_with("workflow started"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn new_captures_paths() {
        let r = GaitWorkflowRunner::new(PathBuf::from("/a"), PathBuf::from("/b"));
        assert_eq!(r.gait_dir, PathBuf::from("/a"));
        assert_eq!(r.codebase, PathBuf::from("/b"));
    }
}
