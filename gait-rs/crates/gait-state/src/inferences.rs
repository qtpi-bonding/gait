//! Authored named inference files.  This is state, rather than core, because
//! every frontend and `gait-ops` must resolve the exact same snapshot.
use crate::config::LlmParamsConfig;
use crate::proto::gait::log::v1 as pb;
use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct InferenceFile {
    model: String,
    compaction_threshold_tokens: u64,
    #[serde(default)]
    llm_params: Option<LlmParamsConfig>,
}

fn validate(
    name: &str,
    model: &str,
    compaction_threshold_tokens: u64,
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
    if compaction_threshold_tokens == 0 {
        anyhow::bail!("inference '{name}' must declare a non-zero compaction_threshold_tokens");
    }
    if let Some(p) = params {
        let mismatch = (p.anthropic.is_some() && provider != "anthropic")
            || (p.openai.is_some() && provider != "openai" && provider != "openai-codex")
            || (p.gemini.is_some() && provider != "gemini")
            || (p.local.is_some() && provider != "local");
        if mismatch {
            anyhow::bail!(
                "inference '{name}' has llm_params for a different provider than '{provider}'"
            );
        }
    }
    if provider == "local" {
        let local = params
            .as_ref()
            .and_then(|p| p.local.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                "inference '{name}' uses local/<model> but is missing llm_params.local.model_path"
            )
            })?;
        if local.model_path.trim().is_empty() {
            anyhow::bail!(
                "inference '{name}' uses local/<model> but llm_params.local.model_path is empty"
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
    validate(name, &f.model, f.compaction_threshold_tokens, &f.llm_params)?;
    Ok(pb::Inference {
        name: name.into(),
        model: f.model,
        compaction_threshold_tokens: f.compaction_threshold_tokens,
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
    compaction_threshold_tokens: u64,
) -> anyhow::Result<()> {
    if name.is_empty() || name == "default" {
        anyhow::bail!("'{name}' is not a valid inference name (reserved)");
    }
    // Validate the model shape up front (no llm_params to mismatch-check here).
    validate(name, model, compaction_threshold_tokens, &None)?;
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
        format!("model: {model}\ncompaction_threshold_tokens: {compaction_threshold_tokens}\n"),
    )
    .map_err(|e| anyhow::anyhow!("write {}: {e}", inf_file.display()))?;
    Ok(())
}

/// Write the explicitly configured starter local profile. The GGUF remains
/// developer-owned: this records a path but never downloads, moves, or
/// modifies that file.
pub fn write_local_qwen_inference(
    gait_dir: &Path,
    name: &str,
    model_path: &Path,
    context_tokens: u32,
    gpu_layers: u32,
    compaction_threshold_tokens: u64,
) -> anyhow::Result<()> {
    let model = "local/qwen2.5-coder-7b-instruct-q4-k-m";
    let params = LlmParamsConfig {
        max_tokens: Some(1024),
        local: Some(crate::config::LocalLlmParamsConfig {
            model_path: model_path.to_string_lossy().into_owned(),
            context_tokens: Some(context_tokens),
            gpu_layers: Some(gpu_layers),
        }),
        ..Default::default()
    };
    validate(
        name,
        model,
        compaction_threshold_tokens,
        &Some(params.clone()),
    )?;
    if name.is_empty() || name == "default" {
        anyhow::bail!("'{name}' is not a valid inference name (reserved)");
    }
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
    let rendered = serde_yaml::to_string(&InferenceFile {
        model: model.into(),
        compaction_threshold_tokens,
        llm_params: Some(params),
    })?;
    std::fs::write(&inf_file, rendered)
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
        compaction_threshold_tokens: 0,
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
            "model: opencode/kimi-k2.6\ncompaction_threshold_tokens: 128000\n"
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
            "model: anthropic/claude-3-5-sonnet\ncompaction_threshold_tokens: 128000\n"
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

    #[test]
    fn local_inference_requires_and_persists_typed_model_path() {
        let dir = ws();
        let path = dir.path().join("models/qwen.gguf");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let inf_dir = dir.path().join("inferences/local");
        std::fs::create_dir_all(&inf_dir).unwrap();
        std::fs::write(
            inf_dir.join("INFERENCE.yaml"),
            format!(
                "model: local/qwen2.5-coder-7b-instruct-q4-k-m\ncompaction_threshold_tokens: 6000\nllm_params:\n  max_tokens: 1024\n  local:\n    model_path: {}\n    context_tokens: 8192\n    gpu_layers: 0\n",
                path.display()
            ),
        )
        .unwrap();

        let loaded = load_inference(dir.path(), "local").unwrap();
        let params =
            crate::proto::events::llm_params_from_proto(loaded.llm_params.as_ref().unwrap())
                .unwrap();
        let local = params.local.expect("local settings survive the snapshot");
        assert_eq!(local.model_path, path.to_string_lossy());
        assert_eq!(local.context_tokens, Some(8192));
        assert_eq!(local.gpu_layers, Some(0));
    }

    #[test]
    fn local_inference_without_a_path_is_rejected_at_load_time() {
        let dir = ws();
        let inf_dir = dir.path().join("inferences/local");
        std::fs::create_dir_all(&inf_dir).unwrap();
        std::fs::write(
            inf_dir.join("INFERENCE.yaml"),
            "model: local/qwen\ncompaction_threshold_tokens: 6000\n",
        )
        .unwrap();
        let err = load_inference(dir.path(), "local").unwrap_err();
        assert!(
            format!("{err}").contains("llm_params.local.model_path"),
            "got: {err}"
        );
    }

    #[test]
    fn write_local_qwen_inference_writes_cpu_safe_typed_settings() {
        let dir = ws();
        let model = dir.path().join("models/qwen.gguf");
        write_local_qwen_inference(dir.path(), "local", &model, 8192, 0, 6000).unwrap();
        let yaml =
            std::fs::read_to_string(dir.path().join("inferences/local/INFERENCE.yaml")).unwrap();
        assert!(yaml.contains("model: local/qwen2.5-coder-7b-instruct-q4-k-m"));
        assert!(yaml.contains("model_path:"));
        let loaded = load_inference(dir.path(), "local").unwrap();
        let params =
            crate::proto::events::llm_params_from_proto(loaded.llm_params.as_ref().unwrap())
                .unwrap();
        let local = params.local.unwrap();
        assert_eq!(local.context_tokens, Some(8192));
        assert_eq!(local.gpu_layers, Some(0));
        assert_eq!(params.max_tokens, Some(1024));
    }
}
