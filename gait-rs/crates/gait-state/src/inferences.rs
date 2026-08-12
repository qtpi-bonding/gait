//! Authored named inference files.  This is state, rather than core, because
//! every frontend and `gait-ops` must resolve the exact same snapshot.
use crate::config::LlmParamsConfig;
use crate::proto::gait::log::v1 as pb;
use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize)]
struct InferenceFile {
    model: String,
    context_window_tokens: u64,
    #[serde(default)]
    llm_params: Option<LlmParamsConfig>,
}

fn validate(
    name: &str,
    model: &str,
    context_window_tokens: u64,
    params: &Option<LlmParamsConfig>,
) -> anyhow::Result<()> {
    let (provider, id) = model.split_once('/').ok_or_else(|| {
        anyhow::anyhow!(
            "inference '{name}' has invalid model '{model}': expected provider/model-id"
        )
    })?;
    if provider.is_empty() || id.is_empty() || id.contains('/') {
        anyhow::bail!("inference '{name}' has invalid model '{model}': expected provider/model-id");
    }
    if context_window_tokens == 0 {
        anyhow::bail!("inference '{name}' must declare a non-zero context_window_tokens");
    }
    if let Some(p) = params {
        let mismatch = (p.anthropic.is_some() && provider != "anthropic")
            || (p.openai.is_some() && provider != "openai" && provider != "openai-codex")
            || (p.gemini.is_some() && provider != "gemini");
        if mismatch {
            anyhow::bail!(
                "inference '{name}' has llm_params for a different provider than '{provider}'"
            );
        }
    }
    Ok(())
}

pub fn load_inference(gait_dir: &Path, name: &str) -> anyhow::Result<pb::Inference> {
    if name.is_empty() || name == "default" {
        anyhow::bail!("'default' is not an authored inference");
    }
    let path = gait_dir
        .join("inferences")
        .join(name)
        .join("INFERENCE.yaml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("no such inference '{name}' ({}): {e}", path.display()))?;
    let f: InferenceFile = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("inference '{name}' is malformed: {e}"))?;
    validate(name, &f.model, f.context_window_tokens, &f.llm_params)?;
    Ok(pb::Inference {
        name: name.into(),
        model: f.model,
        context_window_tokens: f.context_window_tokens,
        llm_params: f
            .llm_params
            .as_ref()
            .map(crate::proto::events::llm_params_to_proto),
    })
}

/// Write `<gait_dir>/inferences/<name>/INFERENCE.yaml` with explicit context
/// metadata and no `llm_params` block — callers who want provider knob tuning edit
/// the YAML in place (`vi inferences/<name>/INFERENCE.yaml`).
///
/// **Refuses overwrite**: if `<name>/INFERENCE.yaml` already exists, errors.
/// Pick a new name or `rm -rf inferences/<name>/` first — never silently
/// clobber a profile the user might be hand-tuning.
///
/// **Validates at write time** so a malformed `provider/id` or a reserved
/// name surfaces here, not on the first turn that tries to load the inference.
pub fn write_inference(
    gait_dir: &Path,
    name: &str,
    model: &str,
    context_window_tokens: u64,
) -> anyhow::Result<()> {
    if name.is_empty() || name == "default" {
        anyhow::bail!("'{name}' is not a valid inference name (reserved)");
    }
    // Validate the model shape up front (no llm_params to mismatch-check here).
    validate(name, model, context_window_tokens, &None)?;
    let inf_dir = gait_dir.join("inferences").join(name);
    let inf_file = inf_dir.join("INFERENCE.yaml");
    if inf_file.exists() {
        anyhow::bail!(
            "inference '{name}' already exists at {}; pick a new name or `rm -rf {}` first",
            inf_file.display(),
            inf_dir.display()
        );
    }
    std::fs::create_dir_all(&inf_dir)?;
    std::fs::write(
        &inf_file,
        format!("model: {model}\ncontext_window_tokens: {context_window_tokens}\n"),
    )
    .map_err(|e| anyhow::anyhow!("write {}: {e}", inf_file.display()))?;
    Ok(())
}

pub fn list_inferences(gait_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(gait_dir.join("inferences")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name != "default" && e.path().join("INFERENCE.yaml").is_file() {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

pub fn clear_inference() -> pb::Inference {
    pb::Inference {
        name: String::new(),
        model: String::new(),
        context_window_tokens: 0,
        llm_params: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn write_inference_then_load_round_trips() {
        let dir = ws();
        write_inference(dir.path(), "opencode", "opencode/kimi-k2.6", 128_000).unwrap();

        let loaded = load_inference(dir.path(), "opencode").unwrap();
        assert_eq!(loaded.name, "opencode");
        assert_eq!(loaded.model, "opencode/kimi-k2.6");
        assert!(
            loaded.llm_params.is_none(),
            "bare write leaves no llm_params"
        );

        // On-disk contents are the minimal single-line YAML.
        let text =
            std::fs::read_to_string(dir.path().join("inferences/opencode/INFERENCE.yaml")).unwrap();
        assert_eq!(
            text,
            "model: opencode/kimi-k2.6\ncontext_window_tokens: 128000\n"
        );
    }

    #[test]
    fn write_inference_refuses_overwrite() {
        let dir = ws();
        write_inference(dir.path(), "p", "anthropic/claude-3-5-sonnet", 128_000).unwrap();
        let err = load_inference(dir.path(), "p").unwrap(); // loads fine the first time
        drop(err);
        let second = write_inference(dir.path(), "p", "openai/gpt-4o", 128_000).unwrap_err();
        // Wraps the `already exists` error.
        assert!(
            format!("{second}").contains("already exists"),
            "got: {second}"
        );
        // And the original file is unchanged — no clobber mid-error.
        let text = std::fs::read_to_string(dir.path().join("inferences/p/INFERENCE.yaml")).unwrap();
        assert_eq!(
            text,
            "model: anthropic/claude-3-5-sonnet\ncontext_window_tokens: 128000\n"
        );
    }

    #[test]
    fn write_inference_rejects_default_name() {
        let dir = ws();
        let err = write_inference(dir.path(), "default", "anthropic/x", 128_000).unwrap_err();
        assert!(format!("{err}").contains("reserved"), "got: {err}");
        // No directory should have been created.
        assert!(!dir.path().join("inferences/default").exists());
    }

    #[test]
    fn write_inference_rejects_empty_name() {
        let dir = ws();
        let err = write_inference(dir.path(), "", "anthropic/x", 128_000).unwrap_err();
        assert!(format!("{err}").contains("reserved"), "got: {err}");
    }

    #[test]
    fn write_inference_rejects_bad_model_shape() {
        let dir = ws();
        // Missing provider/id split.
        let err = write_inference(dir.path(), "p", "noproviderid", 128_000).unwrap_err();
        assert!(format!("{err}").contains("invalid model"), "got: {err}");
        // No directory should have been created on a validation failure.
        assert!(!dir.path().join("inferences/p").exists());
    }

    #[test]
    fn write_inference_rejects_slash_in_id_part() {
        let dir = ws();
        // Provider/id has a '/' in the id half — invalid per `validate`.
        let err = write_inference(dir.path(), "p", "openai/gpt-4o/extra", 128_000).unwrap_err();
        assert!(format!("{err}").contains("invalid model"), "got: {err}");
    }
}
