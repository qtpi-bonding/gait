use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

/// The single model-facing budget for every textual tool result. Complete
/// output is always retained in a session artifact; this only bounds what is
/// copied into the event log and the next provider request.
pub(crate) const MODEL_TOOL_OUTPUT_MAX_BYTES: usize = 40 * 1024;

const ARTIFACT_MARKER_PREFIX: &str = "[full output: ";
const ARTIFACT_ID_PREFIX_CHARS: usize = 8;
const SECTION_SEPARATOR_BYTES: usize = "\n\n".len();

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolOutputArtifact {
    pub path: PathBuf,
    pub bytes: u64,
}

pub(crate) fn prepare_tool_output_path(
    gait_dir: &Path,
    session_id: &str,
    tool_name: &str,
) -> std::io::Result<PathBuf> {
    let call_id = uuid::Uuid::new_v4().simple().to_string();
    let dir = crate::session::live::outputs_dir(gait_dir, session_id)
        .join("tools")
        .join(&call_id[..ARTIFACT_ID_PREFIX_CHARS]);
    std::fs::create_dir_all(&dir)?;
    let safe_name: String = tool_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let safe_name = if safe_name.is_empty() {
        "tool"
    } else {
        &safe_name
    };
    Ok(dir.join(format!("{safe_name}.txt")))
}

pub(crate) fn artifact_for_existing(path: PathBuf) -> std::io::Result<ToolOutputArtifact> {
    let bytes = std::fs::metadata(&path)?.len();
    Ok(ToolOutputArtifact { path, bytes })
}

pub(crate) fn archive_text(path: PathBuf, text: &str) -> std::io::Result<ToolOutputArtifact> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(&path)?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    artifact_for_existing(path)
}

pub(crate) fn archive_value(
    path: PathBuf,
    value: &serde_json::Value,
) -> std::io::Result<ToolOutputArtifact> {
    match value {
        serde_json::Value::String(text) => archive_text(path, text),
        value => archive_text(path, &value.to_string()),
    }
}

pub(crate) fn artifact_marker(artifact: &ToolOutputArtifact) -> String {
    format!(
        "[full output: {} ({} bytes)]",
        artifact.path.display(),
        artifact.bytes
    )
}

/// Recover an artifact emitted by this module from model-facing text. Only an
/// existing file inside the current session's output root is trusted.
pub(crate) fn artifact_from_text(
    text: &str,
    gait_dir: &Path,
    session_id: &str,
) -> Option<ToolOutputArtifact> {
    let start = text.rfind(ARTIFACT_MARKER_PREFIX)? + ARTIFACT_MARKER_PREFIX.len();
    let remainder = &text[start..];
    let (path, suffix) = remainder.rsplit_once(" (")?;
    let bytes = suffix.strip_suffix(" bytes)]")?.parse::<u64>().ok()?;
    let path = PathBuf::from(path);
    let root = crate::session::live::outputs_dir(gait_dir, session_id);
    if !path.starts_with(&root) || std::fs::metadata(&path).ok()?.len() != bytes {
        return None;
    }
    Some(ToolOutputArtifact { path, bytes })
}

pub(crate) fn artifact_from_value(
    value: &serde_json::Value,
    gait_dir: &Path,
    session_id: &str,
) -> Option<ToolOutputArtifact> {
    let text = value
        .get("output")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.as_str())?;
    artifact_from_text(text, gait_dir, session_id)
}

fn read_artifact_preview(artifact: &ToolOutputArtifact, budget: usize) -> std::io::Result<String> {
    if artifact.bytes as usize <= budget {
        return Ok(String::from_utf8_lossy(&std::fs::read(&artifact.path)?).into_owned());
    }

    let mut retained_budget = budget;
    loop {
        let omitted = artifact.bytes.saturating_sub(retained_budget as u64);
        let marker_len = format!("\n... {omitted} bytes omitted ...\n").len();
        let adjusted = budget.saturating_sub(marker_len);
        if adjusted == retained_budget {
            break;
        }
        retained_budget = adjusted;
    }
    let head_budget = retained_budget / 2;
    let tail_budget = retained_budget.saturating_sub(head_budget);
    let omitted = artifact
        .bytes
        .saturating_sub((head_budget + tail_budget) as u64);
    let marker = format!("\n... {omitted} bytes omitted ...\n");
    let mut file = std::fs::File::open(&artifact.path)?;
    let mut head = vec![0; head_budget];
    file.read_exact(&mut head)?;
    file.seek(std::io::SeekFrom::End(-(tail_budget as i64)))?;
    let mut tail = vec![0; tail_budget];
    file.read_exact(&mut tail)?;
    let mut preview = format!(
        "{}{}{}",
        String::from_utf8_lossy(&head),
        marker,
        String::from_utf8_lossy(&tail)
    );
    while preview.len() > budget {
        preview.pop();
    }
    Ok(preview)
}

fn text_head_tail(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let marker = format!(
        "\n... {} bytes omitted ...\n",
        text.len().saturating_sub(budget)
    );
    let retained = budget.saturating_sub(marker.len());
    let mut head_end = retained / 2;
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len().saturating_sub(retained.saturating_sub(head_end));
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{}{}{}", &text[..head_end], marker, &text[tail_start..])
}

pub(crate) fn model_text_for_inline(text: &str, artifact: &ToolOutputArtifact) -> String {
    let footer = artifact_marker(artifact);
    let available =
        MODEL_TOOL_OUTPUT_MAX_BYTES.saturating_sub(footer.len() + SECTION_SEPARATOR_BYTES);
    if text.len() <= available {
        return format!("{text}\n\n{footer}");
    }
    let warning = format!(
        "[Tool output transformed and truncated by Gait: transformed value was {} bytes; complete raw output is in the file below]",
        text.len()
    );
    let preview_budget = MODEL_TOOL_OUTPUT_MAX_BYTES
        .saturating_sub(warning.len() + footer.len() + SECTION_SEPARATOR_BYTES * 2);
    let preview = text_head_tail(text, preview_budget);
    format!("{warning}\n\n{preview}\n\n{footer}")
}

pub(crate) fn model_text_for_artifact(artifact: &ToolOutputArtifact) -> std::io::Result<String> {
    model_text_for_artifact_with_note(artifact, None)
}

pub(crate) fn model_text_for_artifact_with_note(
    artifact: &ToolOutputArtifact,
    note: Option<&str>,
) -> std::io::Result<String> {
    let footer = artifact_marker(artifact);
    let warning = format!(
        "[Tool output truncated by Gait: original was {} bytes; showing head and tail]",
        artifact.bytes
    );
    let note = note.unwrap_or("");
    let note_prefix = if note.is_empty() {
        String::new()
    } else {
        format!("{note}\n")
    };
    let small_budget = MODEL_TOOL_OUTPUT_MAX_BYTES
        .saturating_sub(note_prefix.len() + footer.len() + SECTION_SEPARATOR_BYTES);
    if artifact.bytes as usize <= small_budget {
        let body = read_artifact_preview(artifact, small_budget)?;
        return Ok(format!("{note_prefix}{body}\n\n{footer}"));
    }

    let preview_budget = MODEL_TOOL_OUTPUT_MAX_BYTES.saturating_sub(
        note_prefix.len() + warning.len() + footer.len() + SECTION_SEPARATOR_BYTES * 2,
    );
    let body = read_artifact_preview(artifact, preview_budget)?;
    Ok(format!("{note_prefix}{warning}\n\n{body}\n\n{footer}"))
}

pub(crate) fn model_value_for_artifact(
    mut original: serde_json::Value,
    artifact: &ToolOutputArtifact,
) -> std::io::Result<serde_json::Value> {
    if let serde_json::Value::Object(object) = &mut original {
        if let Some(note) = object.get("output").and_then(serde_json::Value::as_str) {
            let preview = model_text_for_artifact_with_note(artifact, Some(note))?;
            object.insert("output".into(), serde_json::Value::String(preview));
            return Ok(original);
        }
    }
    let preview = model_text_for_artifact(artifact)?;
    Ok(serde_json::Value::String(preview))
}

pub(crate) fn archive_and_prepare_model_value(
    original: serde_json::Value,
    path: PathBuf,
) -> std::io::Result<(serde_json::Value, ToolOutputArtifact)> {
    let artifact = if path.exists() {
        artifact_for_existing(path)?
    } else {
        archive_value(path, &original)?
    };
    let model = model_value_for_artifact(original, &artifact)?;
    Ok((model, artifact))
}

pub(crate) fn ensure_archived_model_value(
    original: serde_json::Value,
    gait_dir: &Path,
    session_id: &str,
    tool_name: &str,
) -> std::io::Result<(serde_json::Value, ToolOutputArtifact)> {
    if let Some(artifact) = artifact_from_value(&original, gait_dir, session_id) {
        return Ok((original, artifact));
    }
    let path = prepare_tool_output_path(gait_dir, session_id, tool_name)?;
    archive_and_prepare_model_value(original, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_keeps_full_text_while_model_gets_head_tail_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let raw = format!(
            "HEAD\n{}\nTAIL",
            "middle".repeat(MODEL_TOOL_OUTPUT_MAX_BYTES)
        );
        let artifact = archive_text(path, &raw).unwrap();
        let model = model_text_for_artifact(&artifact).unwrap();

        assert_eq!(std::fs::read_to_string(&artifact.path).unwrap(), raw);
        assert!(model.contains("HEAD"));
        assert!(model.contains("TAIL"));
        assert!(model.contains("bytes omitted"));
        assert!(model.contains(&artifact.path.display().to_string()));
        assert!(model.len() <= MODEL_TOOL_OUTPUT_MAX_BYTES);
    }
}
