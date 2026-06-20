/// Abstraction over workflow execution. Implemented by `gait-workflow`;
/// defined here in `gait-core` so `RunWorkflowTool` can be a first-class
/// builtin without creating a circular dependency.
///
/// `gait-workflow` provides `GaitWorkflowRunner`. Drivers construct it with
/// the session's `gait_dir` and `codebase`, wrap it in `Arc`, and thread it
/// through `ToolCtx::with_workflow_runner` before calling `Profile::resolve`.
#[async_trait::async_trait]
pub trait WorkflowRunner: Send + Sync + std::fmt::Debug {
    /// Load and asynchronously execute the workflow named `name` from
    /// `{gait_dir}/workflows/{name}.yaml`. Spawns the run in the background
    /// and returns immediately with `"workflow started: <run_id>"` on success,
    /// or `"workflow error: ..."` if the yaml cannot be loaded / parsed.
    async fn run(&self, name: String, args: Vec<String>) -> String;
}
