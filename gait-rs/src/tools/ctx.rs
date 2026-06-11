use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct ToolCtx {
    pub codebase_path: PathBuf,
    pub gait_dir: PathBuf,
    pub session_id: String,
}

impl ToolCtx {
    pub fn new(codebase_path: PathBuf, gait_dir: PathBuf, session_id: String) -> Self {
        Self { codebase_path, gait_dir, session_id }
    }
}
