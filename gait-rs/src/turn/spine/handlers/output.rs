//! The single production finalization boundary for successful tool output.
//!
//! Hands may stream raw bytes into a spine-allocated path, but they never build
//! model previews. In-process tools send their raw value over `ArchiveOutput`.
//! Both paths arrive here, where the spine preserves the complete result and
//! applies the one shared model-facing bound.

use std::path::PathBuf;

use crate::proto::gait::sandbox::v1::{ArchiveOutput, Feedback};

use super::{error_feedback, feedback_result};

/// Finalize a successful tool result while preserving all Feedback sidebands.
/// `preallocated_path` is supplied for a hand invocation; `None` allocates a
/// fresh session-scoped full output for an in-process result.
pub(in crate::turn::spine) fn finalize_feedback(
    mut feedback: Feedback,
    gait_dir: &std::path::Path,
    session_id: &str,
    tool_name: &str,
    preallocated_path: Option<PathBuf>,
) -> Feedback {
    let original = feedback
        .result
        .as_ref()
        .and_then(|value| serde_json::to_value(value).ok())
        .unwrap_or(serde_json::Value::Null);
    let finalized = match preallocated_path {
        Some(path) => crate::tools::archive_and_prepare_model_value(original, path),
        None => {
            crate::tools::ensure_archived_model_value(original, gait_dir, session_id, tool_name)
        }
    };
    match finalized {
        Ok((model_value, _full_output)) => {
            feedback.result = feedback_result(model_value).result;
            feedback
        }
        Err(error) => error_feedback(&format!("failed to archive tool output: {error}")),
    }
}

/// Adapt an in-process brain result to the same finalizer used for hand
/// results. The spine allocates the full output because no hand path exists.
pub(in crate::turn::spine) fn archive_output_feedback(
    request: ArchiveOutput,
    gait_dir: &std::path::Path,
    session_id: &str,
) -> Feedback {
    let feedback = Feedback {
        result: request.result,
        ..Default::default()
    };
    finalize_feedback(feedback, gait_dir, session_id, &request.tool, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_text(feedback: Feedback) -> String {
        let model: serde_json::Value = serde_json::to_value(feedback.result.unwrap()).unwrap();
        model.as_str().unwrap().to_string()
    }

    #[test]
    fn in_process_result_keeps_full_value_and_bounds_model_value() {
        let gait_dir = tempfile::tempdir().unwrap();
        let raw = "line\n".repeat(crate::tools::MODEL_TOOL_OUTPUT_MAX_BYTES);
        let request = ArchiveOutput {
            tool: "load".into(),
            result: Some(serde_json::from_value(serde_json::Value::String(raw.clone())).unwrap()),
        };

        let model = result_text(archive_output_feedback(
            request,
            gait_dir.path(),
            "s-archive",
        ));
        assert!(model.contains("Tool output truncated by Gait"));
        assert!(model.len() <= crate::tools::MODEL_TOOL_OUTPUT_MAX_BYTES);
        let full_output = crate::tools::full_output_from_text(&model, gait_dir.path(), "s-archive")
            .expect("spine should return a valid full-output reference");
        assert_eq!(std::fs::read_to_string(full_output.path).unwrap(), raw);
    }

    #[test]
    fn hand_result_uses_preallocated_stream_file_and_preserves_sidebands() {
        let gait_dir = tempfile::tempdir().unwrap();
        let path = crate::session::live::outputs_dir(gait_dir.path(), "s-hand")
            .join("tools/a1b2c3d4/shell.txt");
        let raw = "line\n".repeat(crate::tools::MODEL_TOOL_OUTPUT_MAX_BYTES);
        crate::tools::archive_text(path.clone(), &raw).unwrap();
        let denied = crate::proto::gait::log::v1::SandboxUnlockItem {
            id: "denial".into(),
            ..Default::default()
        };
        let feedback = Feedback {
            result: Some(
                serde_json::from_value(serde_json::json!({
                    "output": "[cwd: /tmp]",
                    "exit_code": 0
                }))
                .unwrap(),
            ),
            denied: Some(denied.clone()),
            ..Default::default()
        };

        let finalized = finalize_feedback(
            feedback,
            gait_dir.path(),
            "s-hand",
            "shell",
            Some(path.clone()),
        );
        assert_eq!(finalized.denied, Some(denied));
        let model: serde_json::Value = serde_json::to_value(finalized.result.unwrap()).unwrap();
        let output = model["output"].as_str().unwrap();
        assert!(output.contains("Tool output truncated by Gait"));
        assert!(output.contains("[cwd: /tmp]"));
        assert!(output.contains(&path.display().to_string()));
        assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
    }

    #[test]
    fn spine_routed_structured_result_is_archived_before_reply() {
        let gait_dir = tempfile::tempdir().unwrap();
        let original = serde_json::json!({
            "session_id": "schild",
            "status": "done"
        });
        let feedback = feedback_result(original.clone());

        let finalized =
            finalize_feedback(feedback, gait_dir.path(), "s-parent", "spawn_agent", None);
        let model: serde_json::Value = serde_json::to_value(finalized.result.unwrap()).unwrap();
        let full_output =
            crate::tools::full_output_ref_from_value(&model, gait_dir.path(), "s-parent")
                .expect("special spine-routed output must return a valid full-output reference");
        let archived: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(full_output.path).unwrap()).unwrap();
        assert_eq!(archived, original);
    }
}
