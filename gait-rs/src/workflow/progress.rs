use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressFile {
    pub workflow_id: String,
    pub name: String,
    pub status: String,
    pub nodes: Vec<ProgressNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressNode {
    pub id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iter: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routed_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn write_progress(workspace: &Path, session_id: &str, progress: &ProgressFile) -> anyhow::Result<()> {
    let session_dir = workspace.join("sessions").join(session_id);
    let path = session_dir.join("progress.yaml");
    let tmp = session_dir.join("progress.yaml.tmp");
    std::fs::write(&tmp, serde_yaml::to_string(progress)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn read_progress(workspace: &Path, session_id: &str) -> Option<ProgressFile> {
    let path = workspace.join("sessions").join(session_id).join("progress.yaml");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_yaml::from_str(&content).ok()
}

pub fn init_progress(workflow_id: &str, name: &str, node_ids: &[&str]) -> ProgressFile {
    ProgressFile {
        workflow_id: workflow_id.into(),
        name: name.into(),
        status: "running".into(),
        nodes: node_ids.iter().map(|id| ProgressNode {
            id: id.to_string(),
            status: "pending".into(),
            exit: None,
            iter: None,
            routed_to: None,
            paused_at: None,
            error: None,
        }).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_and_read_progress() {
        let ws = TempDir::new().unwrap();
        let session_id = "swf1";
        std::fs::create_dir_all(ws.path().join("sessions").join(session_id)).unwrap();
        let pf = ProgressFile {
            workflow_id: "wf1".into(),
            name: "test-wf".into(),
            status: "running".into(),
            nodes: vec![
                ProgressNode { id: "n1".into(), status: "done".into(),
                    exit: Some(0), iter: None, routed_to: None, paused_at: None, error: None },
                ProgressNode { id: "n2".into(), status: "pending".into(),
                    exit: None, iter: None, routed_to: None, paused_at: None, error: None },
            ],
        };
        write_progress(ws.path(), session_id, &pf).unwrap();
        let read = read_progress(ws.path(), session_id).unwrap();
        assert_eq!(read.workflow_id, "wf1");
        assert_eq!(read.nodes.len(), 2);
        assert_eq!(read.nodes[0].status, "done");
    }
}
