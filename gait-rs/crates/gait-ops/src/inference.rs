//! Named inference operations shared by CLI and REPL.  These functions own
//! session-log mutation and deliberately perform no terminal I/O.
use crate::OpError;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct InferenceInfo {
    pub name: String,
    pub model: String,
}

pub fn list(gait_dir: &Path) -> Vec<String> {
    gait_state::inferences::list_inferences(gait_dir)
}

pub fn show(gait_dir: &Path, name: &str) -> Result<InferenceInfo, OpError> {
    let i = gait_state::inferences::load_inference(gait_dir, name)?;
    Ok(InferenceInfo {
        name: i.name,
        model: i.model,
    })
}

pub fn select(gait_dir: &Path, session_id: &str, name: &str) -> Result<InferenceInfo, OpError> {
    let i = gait_state::inferences::load_inference(gait_dir, name)?;
    append(gait_dir, session_id, i.clone())?;
    Ok(InferenceInfo {
        name: i.name,
        model: i.model,
    })
}

pub fn clear(gait_dir: &Path, session_id: &str) -> Result<(), OpError> {
    append(
        gait_dir,
        session_id,
        gait_state::inferences::clear_inference(),
    )
}

fn append(
    gait_dir: &Path,
    session_id: &str,
    inference: gait_state::gait::log::v1::Inference,
) -> Result<(), OpError> {
    let dir = gait_state::session::live::worktree_dir(gait_dir, session_id);
    std::fs::create_dir_all(&dir).map_err(anyhow::Error::from)?;
    let _lock = gait_state::session::SessionLock::acquire(&dir)?;
    let path = gait_state::session::live::log_path(gait_dir, session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(anyhow::Error::from)?;
    }
    gait_state::wire::append(&path, gait_state::proto::events::inference(inference))?;
    Ok(())
}
