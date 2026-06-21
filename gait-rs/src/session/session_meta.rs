use crate::proto::gait::log::v1::{event, payload};
use crate::session::log::read_events;
use std::path::Path;

/// Session identity derived from the log's `SessionStart` event.
/// Replaces the old `session.json` sidecar — the log is the single source of truth.
#[derive(Debug, Clone, Default)]
pub struct SessionMeta {
    pub id: String,
    pub assigned_by: Option<String>,
    pub codebase_path: Option<String>,
}

/// Read identity from `sessions/<id>/log.jsonl`'s `SessionStart` event.
/// Returns a default (id only) when the log or its `SessionStart` is absent
/// (new session, or a session predating `codebase_path` in `SessionStart`).
pub fn read_session_meta(gait_dir: &Path, session_id: &str) -> anyhow::Result<SessionMeta> {
    let log_path = gait_dir.join("sessions").join(session_id).join("log.jsonl");
    let events = read_events(&log_path)?;
    for e in &events {
        if let Some(event::Body::Payload(p)) = &e.body {
            if let Some(payload::Kind::SessionStart(s)) = &p.kind {
                return Ok(SessionMeta {
                    id: if s.session_id.is_empty() {
                        session_id.to_string()
                    } else {
                        s.session_id.clone()
                    },
                    assigned_by: s.assigned_by.clone(),
                    codebase_path: s.codebase_path.clone(),
                });
            }
        }
    }
    Ok(SessionMeta {
        id: session_id.to_string(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::log::{init_session, InitSessionOpts};
    use tempfile::TempDir;

    #[test]
    fn reads_codebase_path_and_assigned_by_from_log() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("sessions/s1234567/log.jsonl");
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        init_session(InitSessionOpts {
            log_path,
            id: "s1234567".to_string(),
            model: String::new(),
            codebase_hash: String::new(),
            codebase_dirty: false,
            assigned_by: Some("sparent0".to_string()),
            codebase_path: Some("/code".to_string()),
        })
        .unwrap();
        let meta = read_session_meta(dir.path(), "s1234567").unwrap();
        assert_eq!(meta.codebase_path.as_deref(), Some("/code"));
        assert_eq!(meta.assigned_by.as_deref(), Some("sparent0"));
    }

    #[test]
    fn defaults_when_no_log() {
        let dir = TempDir::new().unwrap();
        let meta = read_session_meta(dir.path(), "snolog00").unwrap();
        assert_eq!(meta.id, "snolog00");
        assert!(meta.codebase_path.is_none());
        assert!(meta.assigned_by.is_none());
    }
}
