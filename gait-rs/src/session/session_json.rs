use crate::session::SessionError;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionJson {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_by: Option<String>,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codebase_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

impl SessionJson {
    fn default_for(session_id: &str) -> Self {
        SessionJson {
            id: session_id.to_string(),
            assigned_by: None,
            created_at: 0,
            codebase_path: None,
            agent_name: None,
            system_prompt: None,
        }
    }
}

/// Read `sessions/<session_id>/session.json` from the given gait dir.
/// Returns `Ok(default)` if the file does not exist (new session).
/// Returns `Err` on I/O failure or parse error.
pub fn read_session_json(gait_dir: &Path, session_id: &str) -> Result<SessionJson, SessionError> {
    let path = gait_dir
        .join("sessions")
        .join(session_id)
        .join("session.json");
    if !path.exists() {
        return Ok(SessionJson::default_for(session_id));
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&text)?)
}

pub fn write_session_json(gait_dir: &Path, session_json: &SessionJson) -> anyhow::Result<()> {
    let dir = gait_dir.join("sessions").join(&session_json.id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("session.json");
    let tmp = dir.join("session.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(session_json)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_workspace(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("sessions/stest001")).unwrap();
    }

    #[test]
    fn session_json_roundtrips_through_disk() {
        let dir = TempDir::new().unwrap();
        make_workspace(dir.path());
        let sj = SessionJson {
            id: "stest001".into(),
            assigned_by: Some("sparent".into()),
            created_at: 9999,
            codebase_path: Some("/code".into()),
            agent_name: None,
            system_prompt: None,
        };
        write_session_json(dir.path(), &sj).unwrap();
        let loaded = read_session_json(dir.path(), "stest001").unwrap();
        assert_eq!(loaded.id, "stest001");
        assert_eq!(loaded.assigned_by.as_deref(), Some("sparent"));
    }

    #[test]
    fn read_missing_session_json_returns_default() {
        let dir = TempDir::new().unwrap();
        let sj = read_session_json(dir.path(), "snonexistent").unwrap();
        assert_eq!(sj.id, "snonexistent");
        assert!(sj.assigned_by.is_none());
    }

    #[test]
    fn write_is_atomic_via_rename() {
        // Write then immediately read — should see the written version.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sessions/satomic")).unwrap();
        let sj = SessionJson {
            id: "satomic".into(),
            assigned_by: None,
            created_at: 1,
            codebase_path: None,
            agent_name: None,
            system_prompt: None,
        };
        write_session_json(dir.path(), &sj).unwrap();
        let loaded = read_session_json(dir.path(), "satomic").unwrap();
        assert_eq!(loaded.id, "satomic");
        // Verify there's no leftover .tmp file
        assert!(!dir
            .path()
            .join("sessions/satomic/session.json.tmp")
            .exists());
    }
}
