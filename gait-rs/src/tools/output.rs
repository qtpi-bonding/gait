use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

/// The single model-facing budget for every textual tool result. Complete
/// output is always retained in a session full-output file; this only bounds what is
/// copied into the event log and the next provider request.
pub(crate) const MODEL_TOOL_OUTPUT_MAX_BYTES: usize = 40 * 1024;

const FULL_OUTPUT_MARKER_PREFIX: &str = "[full output: ";
const FULL_OUTPUT_ID_PREFIX_CHARS: usize = 8;
const SECTION_SEPARATOR_BYTES: usize = "\n\n".len();

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FullOutput {
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
        .join(&call_id[..FULL_OUTPUT_ID_PREFIX_CHARS]);
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

pub(crate) fn full_output_for_existing(path: PathBuf) -> std::io::Result<FullOutput> {
    let bytes = std::fs::metadata(&path)?.len();
    Ok(FullOutput { path, bytes })
}

pub(crate) fn archive_text(path: PathBuf, text: &str) -> std::io::Result<FullOutput> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(&path)?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    full_output_for_existing(path)
}

pub(crate) fn archive_value(
    path: PathBuf,
    value: &serde_json::Value,
) -> std::io::Result<FullOutput> {
    match value {
        serde_json::Value::String(text) => archive_text(path, text),
        value => archive_text(path, &canonical_json(value).to_string()),
    }
}

/// Proto Struct fields are map-backed and do not preserve the insertion order
/// of an originating serde_json object. Sort object keys recursively so the
/// archived representation of the same structured value is stable across runs.
fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonical_json).collect())
        }
        serde_json::Value::Object(object) => {
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonical_json(value)))
                    .collect(),
            )
        }
        value => value.clone(),
    }
}

pub(crate) fn full_output_marker(full_output: &FullOutput) -> String {
    format!(
        "[full output: {} ({} bytes)]",
        full_output.path.display(),
        full_output.bytes
    )
}

pub(crate) fn full_output_ref_from_text(
    text: &str,
    gait_dir: &Path,
    session_id: &str,
) -> Option<FullOutput> {
    let start = text.rfind(FULL_OUTPUT_MARKER_PREFIX)? + FULL_OUTPUT_MARKER_PREFIX.len();
    let remainder = &text[start..];
    let (path, suffix) = remainder.rsplit_once(" (")?;
    let bytes = suffix.strip_suffix(" bytes)]")?.parse::<u64>().ok()?;
    let path = PathBuf::from(path);
    let root = crate::session::live::outputs_dir(gait_dir, session_id);
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
        || !path.starts_with(&root)
    {
        return None;
    }
    Some(FullOutput { path, bytes })
}

/// Recover a full-output file emitted by this module from model-facing text. Only an
/// existing file inside the current session's output root is trusted.
pub(crate) fn full_output_from_text(
    text: &str,
    gait_dir: &Path,
    session_id: &str,
) -> Option<FullOutput> {
    let full_output = full_output_ref_from_text(text, gait_dir, session_id)?;
    if std::fs::metadata(&full_output.path).ok()?.len() != full_output.bytes {
        return None;
    }
    Some(full_output)
}

pub(crate) fn full_output_from_value(
    value: &serde_json::Value,
    gait_dir: &Path,
    session_id: &str,
) -> Option<FullOutput> {
    let text = value
        .get("output")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.as_str())?;
    full_output_from_text(text, gait_dir, session_id)
}

/// Recover the hand-finalized full-output reference without touching the
/// filesystem. The sealed brain uses this only after a RelaySynapse response;
/// the hand generated the last marker after archiving the complete output.
pub(crate) fn full_output_ref_from_value(
    value: &serde_json::Value,
    gait_dir: &Path,
    session_id: &str,
) -> Option<FullOutput> {
    let text = value
        .get("output")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.as_str())?;
    full_output_ref_from_text(text, gait_dir, session_id)
}

fn read_full_output_preview(full_output: &FullOutput, budget: usize) -> std::io::Result<String> {
    if full_output.bytes as usize <= budget {
        return Ok(String::from_utf8_lossy(&std::fs::read(&full_output.path)?).into_owned());
    }

    let mut retained_budget = budget;
    loop {
        let omitted = full_output.bytes.saturating_sub(retained_budget as u64);
        let marker_len = format!("\n... {omitted} bytes omitted ...\n").len();
        let adjusted = budget.saturating_sub(marker_len);
        if adjusted == retained_budget {
            break;
        }
        retained_budget = adjusted;
    }
    let head_budget = retained_budget / 2;
    let tail_budget = retained_budget.saturating_sub(head_budget);
    let omitted = full_output
        .bytes
        .saturating_sub((head_budget + tail_budget) as u64);
    let marker = format!("\n... {omitted} bytes omitted ...\n");
    let mut file = std::fs::File::open(&full_output.path)?;
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

pub(crate) fn model_text_for_inline(text: &str, full_output: &FullOutput) -> String {
    let footer = full_output_marker(full_output);
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

pub(crate) fn model_text_for_inline_with_note(
    text: &str,
    note: &str,
    full_output: &FullOutput,
) -> String {
    let footer = full_output_marker(full_output);
    let suffix = format!("\n\n{footer}");
    let body = text.strip_suffix(&suffix).unwrap_or(text);
    model_text_for_inline(&format!("{note}\n{body}"), full_output)
}

pub(crate) fn model_text_for_full_output(full_output: &FullOutput) -> std::io::Result<String> {
    model_text_for_full_output_with_note(full_output, None)
}

pub(crate) fn model_text_for_full_output_with_note(
    full_output: &FullOutput,
    note: Option<&str>,
) -> std::io::Result<String> {
    let footer = full_output_marker(full_output);
    let warning = format!(
        "[Tool output truncated by Gait: original was {} bytes; showing head and tail]",
        full_output.bytes
    );
    let note = note.unwrap_or("");
    let note_prefix = if note.is_empty() {
        String::new()
    } else {
        format!("{note}\n")
    };
    let small_budget = MODEL_TOOL_OUTPUT_MAX_BYTES
        .saturating_sub(note_prefix.len() + footer.len() + SECTION_SEPARATOR_BYTES);
    if full_output.bytes as usize <= small_budget {
        let body = read_full_output_preview(full_output, small_budget)?;
        return Ok(format!("{note_prefix}{body}\n\n{footer}"));
    }

    let preview_budget = MODEL_TOOL_OUTPUT_MAX_BYTES.saturating_sub(
        note_prefix.len() + warning.len() + footer.len() + SECTION_SEPARATOR_BYTES * 2,
    );
    let body = read_full_output_preview(full_output, preview_budget)?;
    Ok(format!("{note_prefix}{warning}\n\n{body}\n\n{footer}"))
}

pub(crate) fn model_value_for_full_output(
    mut original: serde_json::Value,
    full_output: &FullOutput,
) -> std::io::Result<serde_json::Value> {
    if let serde_json::Value::Object(object) = &mut original {
        if let Some(note) = object.get("output").and_then(serde_json::Value::as_str) {
            let preview = model_text_for_full_output_with_note(full_output, Some(note))?;
            object.insert("output".into(), serde_json::Value::String(preview));
            return Ok(original);
        }
    }
    let preview = model_text_for_full_output(full_output)?;
    Ok(serde_json::Value::String(preview))
}

pub(crate) fn archive_and_prepare_model_value(
    original: serde_json::Value,
    path: PathBuf,
) -> std::io::Result<(serde_json::Value, FullOutput)> {
    let full_output = if path.exists() {
        full_output_for_existing(path)?
    } else {
        archive_value(path, &original)?
    };
    let model = model_value_for_full_output(original, &full_output)?;
    Ok((model, full_output))
}

pub(crate) fn ensure_archived_model_value(
    original: serde_json::Value,
    gait_dir: &Path,
    session_id: &str,
    tool_name: &str,
) -> std::io::Result<(serde_json::Value, FullOutput)> {
    if let Some(full_output) = full_output_from_value(&original, gait_dir, session_id) {
        return Ok((original, full_output));
    }
    let path = prepare_tool_output_path(gait_dir, session_id, tool_name)?;
    archive_and_prepare_model_value(original, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_full_outputs_have_canonical_key_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("structured.json");
        let value = serde_json::json!({
            "z": { "second": 2, "first": 1 },
            "a": true
        });

        archive_value(path.clone(), &value).unwrap();

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            r#"{"a":true,"z":{"first":1,"second":2}}"#
        );
    }

    #[test]
    fn full_output_keeps_full_text_while_model_gets_head_tail_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let raw = format!(
            "HEAD\n{}\nTAIL",
            "middle".repeat(MODEL_TOOL_OUTPUT_MAX_BYTES)
        );
        let full_output = archive_text(path, &raw).unwrap();
        let model = model_text_for_full_output(&full_output).unwrap();

        assert_eq!(std::fs::read_to_string(&full_output.path).unwrap(), raw);
        assert!(model.contains("HEAD"));
        assert!(model.contains("TAIL"));
        assert!(model.contains("bytes omitted"));
        assert!(model.contains(&full_output.path.display().to_string()));
        assert!(model.len() <= MODEL_TOOL_OUTPUT_MAX_BYTES);
    }

    #[test]
    fn sealed_consumer_can_validate_hand_reference_without_reading_full_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::session::live::outputs_dir(dir.path(), "s-test")
            .join("tools/a1b2c3d4/shell.txt");
        let value = serde_json::Value::String(format!(
            "preview\n\n[full output: {} (42 bytes)]",
            path.display()
        ));

        let reference = full_output_ref_from_value(&value, dir.path(), "s-test").unwrap();
        assert_eq!(reference, FullOutput { path, bytes: 42 });
        assert!(full_output_from_value(&value, dir.path(), "s-test").is_none());
    }

    #[test]
    fn full_output_reference_rejects_lexical_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::session::live::outputs_dir(dir.path(), "s-test");
        let escaped = root.join("../s-other/outputs/tools/a1b2c3d4/shell.txt");
        let value =
            serde_json::Value::String(format!("[full output: {} (42 bytes)]", escaped.display()));

        assert!(full_output_ref_from_value(&value, dir.path(), "s-test").is_none());
    }

    #[test]
    fn approval_note_rebounds_inline_preview_without_reading_full_output() {
        let dir = tempfile::tempdir().unwrap();
        let full_output = FullOutput {
            path: crate::session::live::outputs_dir(dir.path(), "s-test")
                .join("tools/a1b2c3d4/shell.txt"),
            bytes: (MODEL_TOOL_OUTPUT_MAX_BYTES * 2) as u64,
        };
        let input = format!(
            "{}\n\n{}",
            "x".repeat(MODEL_TOOL_OUTPUT_MAX_BYTES),
            full_output_marker(&full_output)
        );

        let model = model_text_for_inline_with_note(&input, "[User approved.]", &full_output);
        assert!(model.starts_with("[Tool output transformed and truncated by Gait:"));
        assert!(model.contains("[User approved.]"));
        assert!(model.ends_with(&full_output_marker(&full_output)));
        assert!(model.len() <= MODEL_TOOL_OUTPUT_MAX_BYTES);
    }
}
