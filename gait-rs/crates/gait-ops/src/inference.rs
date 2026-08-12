//! Named inference operations shared by CLI and REPL. Selection resolution is
//! pure: frontends submit the returned event through core's session-update
//! port. These functions deliberately perform no terminal I/O.
use crate::OpError;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct InferenceInfo {
    pub name: String,
    pub model: String,
}

/// A fully resolved, immutable session selection. Frontends submit
/// `event_body` to core's session-update port; this operation performs no
/// session initialization or log writes.
#[derive(Debug, Clone)]
pub struct ResolvedInferenceSelection {
    pub info: InferenceInfo,
    pub inference: gait_proto::gait::log::v1::Inference,
    pub event_body: gait_proto::gait::log::v1::event::Body,
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

pub fn resolve_selection(
    gait_dir: &Path,
    name: &str,
) -> Result<ResolvedInferenceSelection, OpError> {
    let inference = if name == "default" {
        gait_state::inferences::clear_inference()
    } else {
        gait_state::inferences::load_inference(gait_dir, name)?
    };
    let info = InferenceInfo {
        name: if inference.name.is_empty() {
            "default".into()
        } else {
            inference.name.clone()
        },
        model: inference.model.clone(),
    };
    Ok(ResolvedInferenceSelection {
        info,
        event_body: gait_state::proto::events::inference(inference.clone()),
        inference,
    })
}

/// Author a new named inference at `<gait_dir>/inferences/<name>/INFERENCE.yaml`
/// with a bare `model: <provider/id>` line (no `llm_params` block). Returns the
/// `InferenceInfo { name, model }` it just wrote so callers can echo it back.
///
/// Refuses overwrite — see `gait_state::inferences::write_inference`. To tune
/// llm_params or anything else after creation, edit the YAML in place.
pub fn add(
    gait_dir: &Path,
    name: &str,
    model: &str,
    compaction_threshold_tokens: u64,
) -> Result<InferenceInfo, OpError> {
    gait_state::inferences::write_inference(gait_dir, name, model, compaction_threshold_tokens)?;
    Ok(InferenceInfo {
        name: name.to_string(),
        model: model.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_proto::gait::log::v1::{event, payload};

    #[test]
    fn resolve_selection_returns_snapshot_without_session_write() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("inferences/codex")).unwrap();
        std::fs::write(
            dir.path().join("inferences/codex/INFERENCE.yaml"),
            "model: openai-codex/gpt-test\ncompaction_threshold_tokens: 128000\nllm_params:\n  openai:\n    reasoning_effort: high\n",
        )
        .unwrap();

        let selection = resolve_selection(dir.path(), "codex").unwrap();
        assert_eq!(selection.info.name, "codex");
        assert_eq!(selection.info.model, "openai-codex/gpt-test");
        let event::Body::Payload(payload) = selection.event_body else {
            panic!("expected payload")
        };
        let Some(payload::Kind::Inference(inference)) = payload.kind else {
            panic!("expected inference")
        };
        assert!(
            inference.llm_params.is_some(),
            "snapshot retains llm_params"
        );
        assert!(
            !dir.path().join(".active").exists(),
            "resolution must not initialize or write a session"
        );
    }

    #[test]
    fn default_resolves_to_clear_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let selection = resolve_selection(dir.path(), "default").unwrap();
        assert_eq!(selection.info.name, "default");
        let event::Body::Payload(payload) = selection.event_body else {
            panic!("expected payload")
        };
        let Some(payload::Kind::Inference(inference)) = payload.kind else {
            panic!("expected inference")
        };
        assert!(inference.name.is_empty());
        assert!(inference.model.is_empty());
    }
}
