//! Authored named inference files.  This is state, rather than core, because
//! every frontend and `gait-ops` must resolve the exact same snapshot.
use crate::config::LlmParamsConfig;
use crate::proto::gait::log::v1 as pb;
use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize)]
struct InferenceFile {
    model: String,
    #[serde(default)]
    llm_params: Option<LlmParamsConfig>,
}

fn validate(name: &str, model: &str, params: &Option<LlmParamsConfig>) -> anyhow::Result<()> {
    let (provider, id) = model.split_once('/').ok_or_else(|| {
        anyhow::anyhow!(
            "inference '{name}' has invalid model '{model}': expected provider/model-id"
        )
    })?;
    if provider.is_empty() || id.is_empty() || id.contains('/') {
        anyhow::bail!("inference '{name}' has invalid model '{model}': expected provider/model-id");
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
    validate(name, &f.model, &f.llm_params)?;
    Ok(pb::Inference {
        name: name.into(),
        model: f.model,
        llm_params: f
            .llm_params
            .as_ref()
            .map(crate::proto::events::llm_params_to_proto),
    })
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
        llm_params: None,
    }
}
