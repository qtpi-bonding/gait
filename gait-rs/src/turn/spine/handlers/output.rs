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
/// fresh session-scoped full output for an in-process result. `presentation`
/// selects which finalizer runs — see `crate::types::OutputPresentation`:
/// `ModelPreview` (today's human-facing truncated bound) for every ordinary
/// caller, or `CodemodeFull` for a Code Mode capability dispatch, which needs
/// the real, complete value materialized here (spine-side, where disk access
/// is real) rather than a marker the sealed brain could never read back.
pub(in crate::turn::spine) fn finalize_feedback(
    mut feedback: Feedback,
    gait_dir: &std::path::Path,
    session_id: &str,
    tool_name: &str,
    preallocated_path: Option<PathBuf>,
    presentation: crate::types::OutputPresentation,
) -> Feedback {
    let original = feedback
        .result
        .as_ref()
        .and_then(|value| serde_json::to_value(value).ok())
        .unwrap_or(serde_json::Value::Null);
    match presentation {
        crate::types::OutputPresentation::ModelPreview => {
            let finalized = match preallocated_path {
                Some(path) => crate::tools::archive_and_prepare_model_value(original, path),
                None => crate::tools::ensure_archived_model_value(
                    original, gait_dir, session_id, tool_name,
                ),
            };
            match finalized {
                Ok((model_value, _full_output)) => {
                    feedback.result = feedback_result(model_value).result;
                    feedback
                }
                Err(error) => error_feedback(&format!("failed to archive tool output: {error}")),
            }
        }
        crate::types::OutputPresentation::CodemodeFull => {
            let path = match preallocated_path {
                Some(path) => path,
                None => {
                    match crate::tools::prepare_tool_output_path(gait_dir, session_id, tool_name) {
                        Ok(path) => path,
                        Err(error) => {
                            return error_feedback(&format!(
                                "failed to prepare tool output: {error}"
                            ))
                        }
                    }
                }
            };
            // Under `min` (no `code-mode` feature) `gait_codemode` isn't
            // linked, but `CodemodeFull` is a plain-data enum variant that
            // still exists structurally — this branch is simply unreachable
            // in practice (no Code Mode tool is ever dispatched, see Task
            // 6c). Mirror `gait_codemode::Limits::default().max_output_bytes`
            // locally so behavior stays identical to the real dependency.
            #[cfg(feature = "code-mode")]
            let max_bytes = gait_codemode::Limits::default().max_output_bytes;
            #[cfg(not(feature = "code-mode"))]
            let max_bytes: usize = 4 * 1024 * 1024;
            match crate::tools::archive_and_prepare_full_value(original, path, max_bytes) {
                Ok(full_value) => {
                    feedback.result = feedback_result(full_value).result;
                    feedback
                }
                Err(error) => {
                    let mut fb = error_feedback(&error.to_string());
                    fb.is_error = Some(true);
                    fb
                }
            }
        }
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
    finalize_feedback(
        feedback,
        gait_dir,
        session_id,
        &request.tool,
        None,
        crate::types::OutputPresentation::ModelPreview,
    )
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
            crate::types::OutputPresentation::ModelPreview,
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

        let finalized = finalize_feedback(
            feedback,
            gait_dir.path(),
            "s-parent",
            "spawn_agent",
            None,
            crate::types::OutputPresentation::ModelPreview,
        );
        let model: serde_json::Value = serde_json::to_value(finalized.result.unwrap()).unwrap();
        let full_output =
            crate::tools::full_output_ref_from_value(&model, gait_dir.path(), "s-parent")
                .expect("special spine-routed output must return a valid full-output reference");
        let archived: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(full_output.path).unwrap()).unwrap();
        assert_eq!(archived, original);
    }

    #[test]
    fn codemode_full_presentation_returns_the_real_value_not_a_truncated_preview() {
        let gait_dir = tempfile::tempdir().unwrap();
        let raw = "line\n".repeat(crate::tools::MODEL_TOOL_OUTPUT_MAX_BYTES);
        let feedback = feedback_result(serde_json::Value::String(raw.clone()));

        let finalized = finalize_feedback(
            feedback,
            gait_dir.path(),
            "s-codemode",
            "read",
            None,
            crate::types::OutputPresentation::CodemodeFull,
        );

        assert!(!finalized.is_error.unwrap_or(false));
        let value: serde_json::Value = serde_json::to_value(finalized.result.unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::Value::String(raw),
            "Code Mode must get the real, complete value -- never the human-facing \
             truncated preview or a marker it can't dereference"
        );
    }

    #[test]
    fn codemode_full_presentation_round_trips_a_structured_value() {
        // exit_code deliberately not a bare integer: Feedback.result is a
        // google.protobuf.Value, which represents every number as a double --
        // the same as JS itself (no int/float distinction), so 0 vs 0.0 is
        // never observable by the actual Code Mode consumer. Use a string
        // field here so the round-trip assertion isn't testing that
        // wire-type detail instead of the archive/reconstruction behavior.
        let gait_dir = tempfile::tempdir().unwrap();
        let original = serde_json::json!({"output": "hi", "exit_code": "0"});
        let feedback = feedback_result(original.clone());

        let finalized = finalize_feedback(
            feedback,
            gait_dir.path(),
            "s-codemode",
            "shell",
            None,
            crate::types::OutputPresentation::CodemodeFull,
        );

        let value: serde_json::Value = serde_json::to_value(finalized.result.unwrap()).unwrap();
        assert_eq!(value, original);
    }

    // References `gait_codemode::Limits` directly to prove the finalizer
    // bounds against the *real* Code Mode budget, not a test-only stand-in —
    // meaningless (and unlinked) without the `code-mode` feature.
    #[cfg(feature = "code-mode")]
    #[test]
    fn codemode_full_presentation_errors_unambiguously_when_over_budget() {
        // Genuinely exceed the real Code Mode output budget (not a fake
        // injected one) -- this proves the finalizer bounds against the same
        // limit production code uses, not a test-only stand-in.
        let gait_dir = tempfile::tempdir().unwrap();
        let oversized = "x".repeat(gait_codemode::Limits::default().max_output_bytes + 1);
        let feedback = feedback_result(serde_json::Value::String(oversized));

        let finalized = finalize_feedback(
            feedback,
            gait_dir.path(),
            "s-codemode",
            "read",
            None,
            crate::types::OutputPresentation::CodemodeFull,
        );

        assert_eq!(
            finalized.is_error,
            Some(true),
            "an oversized archive must be an unambiguous error, not a value \
             indistinguishable from a successful capability result"
        );
    }
}
