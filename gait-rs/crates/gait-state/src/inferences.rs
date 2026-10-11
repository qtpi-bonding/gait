//! Authored named inference profiles, stored as protobuf text format.
use crate::config::LlmParamsConfig;
use crate::proto::gait::log::v1 as pb;
use gait_proto::textproto::{inference_to_text, parse_inference_text};
use std::path::Path;

fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        anyhow::bail!("inference name '{name}' is invalid: must be a single normal path component");
    }
    Ok(())
}

fn validate(
    name: &str,
    model: &str,
    threshold: u64,
    params: &Option<pb::LlmParams>,
) -> anyhow::Result<()> {
    let (provider, id) = model.split_once('/').ok_or_else(|| {
        anyhow::anyhow!(
            "inference '{name}' has invalid model '{model}': expected provider/model-id"
        )
    })?;
    if provider.is_empty() || id.is_empty() || id.contains('/') {
        anyhow::bail!("inference '{name}' has invalid model '{model}': expected provider/model-id");
    }
    if threshold == 0 {
        anyhow::bail!("inference '{name}' must declare a non-zero compaction_threshold_tokens");
    }
    if let Some(p) = params {
        if let Some(pp) = &p.provider_params {
            let ok = matches!(
                (provider, pp),
                ("anthropic", pb::llm_params::ProviderParams::Anthropic(_))
                    | (
                        "openai" | "openai-codex",
                        pb::llm_params::ProviderParams::Openai(_)
                    )
                    | ("gemini", pb::llm_params::ProviderParams::Gemini(_))
                    | ("local", pb::llm_params::ProviderParams::Local(_))
            );
            if !ok {
                anyhow::bail!(
                    "inference '{name}' has llm_params for a different provider than '{provider}'"
                );
            }
        }
    }
    if provider == "local" {
        match params.as_ref().and_then(|p| match &p.provider_params {
            Some(pb::llm_params::ProviderParams::Local(x)) => Some(x),
            _ => None,
        }) {
            Some(local) if !local.model_path.trim().is_empty() => {}
            Some(_) => anyhow::bail!(
                "inference '{name}' uses local/<model> but llm_params.local.model_path is empty"
            ),
            None => anyhow::bail!(
                "inference '{name}' uses local/<model> but is missing llm_params.local.model_path"
            ),
        }
    }
    Ok(())
}

pub fn load_inference(gait_dir: &Path, name: &str) -> anyhow::Result<pb::Inference> {
    validate_name(name)?;
    if name == "default" {
        anyhow::bail!("'default' is not an authored inference");
    }
    let path = gait_dir
        .join("inferences")
        .join(name)
        .join("inference.textproto");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let yaml = path.with_file_name("INFERENCE.yaml");
            if yaml.is_file() {
                anyhow::bail!(
                    "inference '{name}' uses the removed INFERENCE.yaml format; rewrite it as inference.textproto (model: \"provider/model-id\" and compaction_threshold_tokens: N)"
                );
            }
            anyhow::bail!("no such inference '{name}' ({}): {e}", path.display());
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "no such inference '{name}' ({}): {e}",
                path.display()
            ))
        }
    };
    let mut inf = parse_inference_text(&text)
        .map_err(|e| anyhow::anyhow!("inference '{name}' is malformed: {e}"))?;
    if !inf.name.is_empty() && inf.name != name {
        anyhow::bail!("inference '{name}' declares name '{}'", inf.name);
    }
    inf.name = name.into();
    validate(
        name,
        &inf.model,
        inf.compaction_threshold_tokens,
        &inf.llm_params,
    )?;
    Ok(inf)
}

pub fn write_inference(
    gait_dir: &Path,
    name: &str,
    model: &str,
    threshold: u64,
) -> anyhow::Result<()> {
    validate_name(name)?;
    if name == "default" {
        anyhow::bail!("'{name}' is not a valid inference name (reserved)");
    }
    validate(name, model, threshold, &None)?;
    let dir = gait_dir.join("inferences").join(name);
    let path = dir.join("inference.textproto");
    if path.exists() {
        anyhow::bail!(
            "inference '{name}' already exists at {}; pick a new name or `rm -rf {}` first",
            path.display(),
            dir.display()
        );
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        &path,
        inference_to_text(&pb::Inference {
            name: name.into(),
            model: model.into(),
            compaction_threshold_tokens: threshold,
            llm_params: None,
        }),
    )?;
    Ok(())
}

pub fn write_local_qwen_inference(
    gait_dir: &Path,
    name: &str,
    model_path: &Path,
    context_tokens: u32,
    gpu_layers: u32,
    threshold: u64,
) -> anyhow::Result<()> {
    validate_name(name)?;
    let params = pb::LlmParams {
        max_tokens: Some(1024),
        provider_params: Some(pb::llm_params::ProviderParams::Local(pb::LocalLlmParams {
            model_path: model_path.to_string_lossy().into(),
            context_tokens: Some(context_tokens),
            gpu_layers: Some(gpu_layers),
        })),
        ..Default::default()
    };
    let model = "local/qwen2.5-coder-7b-instruct-q4-k-m";
    validate(name, model, threshold, &Some(params.clone()))?;
    if name == "default" {
        anyhow::bail!("'{name}' is not a valid inference name (reserved)");
    }
    let dir = gait_dir.join("inferences").join(name);
    let path = dir.join("inference.textproto");
    if path.exists() {
        anyhow::bail!("inference '{name}' already exists at {}", path.display());
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        path,
        inference_to_text(&pb::Inference {
            name: name.into(),
            model: model.into(),
            compaction_threshold_tokens: threshold,
            llm_params: Some(params),
        }),
    )?;
    Ok(())
}

pub fn list_inferences(gait_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(gait_dir.join("inferences")) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n != "default" && e.path().join("inference.textproto").is_file() {
                out.push(n);
            }
        }
    }
    out.sort();
    out
}

pub fn clear_inference() -> pb::Inference {
    pb::Inference::default()
}

// Kept for provider/config consumers; profile persistence uses the proto directly.
pub fn config_to_proto(p: &LlmParamsConfig) -> pb::LlmParams {
    crate::proto::events::llm_params_to_proto(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_proto::gait::log::v1::llm_params::ProviderParams;

    fn profile(dir: &tempfile::TempDir, name: &str, body: &str) {
        let path = dir.path().join("inferences").join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("inference.textproto"), body).unwrap();
    }

    #[test]
    fn providers_round_trip_and_strict_tools_survives() {
        let dir = tempfile::tempdir().unwrap();
        profile(&dir, "openai", "model: \"openai/gpt\"\ncompaction_threshold_tokens: 1\nllm_params { openai { reasoning_effort: \"high\" } }\n");
        profile(&dir, "anthropic", "model: \"anthropic/claude\"\ncompaction_threshold_tokens: 1\nllm_params { anthropic { effort: \"high\" strict_tools: true } }\n");
        profile(
            &dir,
            "gemini",
            "model: \"gemini/gem\"\ncompaction_threshold_tokens: 1\nllm_params { gemini {} }\n",
        );
        profile(&dir, "local", "model: \"local/qwen\"\ncompaction_threshold_tokens: 1\nllm_params { local { model_path: \"/tmp/qwen.gguf\" } }\n");
        assert_eq!(
            load_inference(dir.path(), "openai").unwrap().model,
            "openai/gpt"
        );
        assert!(
            matches!(load_inference(dir.path(), "anthropic").unwrap().llm_params.unwrap().provider_params,
            Some(ProviderParams::Anthropic(a)) if a.strict_tools == Some(true))
        );
        assert_eq!(
            load_inference(dir.path(), "local")
                .unwrap()
                .llm_params
                .unwrap()
                .provider_params
                .unwrap(),
            ProviderParams::Local(pb::LocalLlmParams {
                model_path: "/tmp/qwen.gguf".into(),
                ..Default::default()
            })
        );
    }

    #[test]
    fn each_validate_rule_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body, expected) in [
            ("no-slash", "model: \"bad\"\ncompaction_threshold_tokens: 1\n", "expected provider/model-id"),
            ("empty-provider", "model: \"/id\"\ncompaction_threshold_tokens: 1\n", "expected provider/model-id"),
            ("empty-id", "model: \"provider/\"\ncompaction_threshold_tokens: 1\n", "expected provider/model-id"),
            ("slash-in-id", "model: \"provider/a/b\"\ncompaction_threshold_tokens: 1\n", "expected provider/model-id"),
            ("zero", "model: \"openai/gpt\"\ncompaction_threshold_tokens: 0\n", "non-zero compaction_threshold_tokens"),
            ("anthropic", "model: \"openai/gpt\"\ncompaction_threshold_tokens: 1\nllm_params { anthropic {} }\n", "different provider"),
            ("openai", "model: \"anthropic/claude\"\ncompaction_threshold_tokens: 1\nllm_params { openai {} }\n", "different provider"),
            ("gemini", "model: \"openai/gpt\"\ncompaction_threshold_tokens: 1\nllm_params { gemini {} }\n", "different provider"),
            ("local-provider", "model: \"openai/gpt\"\ncompaction_threshold_tokens: 1\nllm_params { local { model_path: \"/tmp/x\" } }\n", "different provider"),
            ("local-missing", "model: \"local/qwen\"\ncompaction_threshold_tokens: 1\n", "missing llm_params.local.model_path"),
            ("local-empty", "model: \"local/qwen\"\ncompaction_threshold_tokens: 1\nllm_params { local {} }\n", "model_path is empty"),
        ] {
            profile(&dir, name, body);
            let error = load_inference(dir.path(), name).unwrap_err().to_string();
            assert!(error.contains(expected), "{name}: {error}");
        }
        profile(
            &dir,
            "codex",
            "model: \"openai-codex/x\"\ncompaction_threshold_tokens: 1\nllm_params { openai {} }\n",
        );
        assert_eq!(
            load_inference(dir.path(), "codex").unwrap().model,
            "openai-codex/x"
        );
    }

    #[test]
    fn name_mismatch_and_stale_yaml_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        profile(
            &dir,
            "right",
            "name: \"wrong\"\nmodel: \"openai/gpt\"\ncompaction_threshold_tokens: 1\n",
        );
        assert_eq!(
            load_inference(dir.path(), "right").unwrap_err().to_string(),
            "inference 'right' declares name 'wrong'"
        );
        profile(
            &dir,
            "equal",
            "name: \"equal\"\nmodel: \"openai/gpt\"\ncompaction_threshold_tokens: 1\n",
        );
        assert_eq!(load_inference(dir.path(), "equal").unwrap().name, "equal");
        let stale = dir.path().join("inferences/stale");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("INFERENCE.yaml"), "model: old/x\n").unwrap();
        let error = load_inference(dir.path(), "stale").unwrap_err().to_string();
        assert!(error.contains(
            "inference 'stale' uses the removed INFERENCE.yaml format; rewrite it as inference.textproto"
        ));
        profile(
            &dir,
            "listed",
            "model: \"openai/gpt\"\ncompaction_threshold_tokens: 1\n",
        );
        let listed = list_inferences(dir.path());
        assert!(listed.contains(&"listed".to_string()));
        assert!(listed.contains(&"right".to_string()));
        assert!(!listed.contains(&"stale".to_string()));
    }

    #[test]
    fn writers_round_trip_and_refuse_second_write() {
        let dir = tempfile::tempdir().unwrap();
        write_local_qwen_inference(
            dir.path(),
            "qwen",
            Path::new("/tmp/qwen.gguf"),
            8192,
            32,
            777,
        )
        .unwrap();
        let qwen = load_inference(dir.path(), "qwen").unwrap();
        assert_eq!(qwen.model, "local/qwen2.5-coder-7b-instruct-q4-k-m");
        assert_eq!(qwen.compaction_threshold_tokens, 777);
        let params = qwen.llm_params.unwrap();
        assert_eq!(params.max_tokens, Some(1024));
        assert_eq!(
            params.provider_params,
            Some(ProviderParams::Local(pb::LocalLlmParams {
                model_path: "/tmp/qwen.gguf".into(),
                context_tokens: Some(8192),
                gpu_layers: Some(32),
            }))
        );

        write_inference(dir.path(), "plain", "openai/gpt", 42).unwrap();
        assert_eq!(
            load_inference(dir.path(), "plain").unwrap().model,
            "openai/gpt"
        );
        let error = write_inference(dir.path(), "plain", "openai/other", 42)
            .unwrap_err()
            .to_string();
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn unsafe_names_are_rejected_without_escape() {
        let dir = tempfile::tempdir().unwrap();
        let names = ["..", "../x", "a/b", "a\\b", "", "."];
        for name in names {
            let load_error = load_inference(dir.path(), name).unwrap_err().to_string();
            assert!(
                load_error.contains("single normal path component"),
                "{name:?}: {load_error}"
            );
            let write_error = write_inference(dir.path(), name, "openai/gpt", 1)
                .unwrap_err()
                .to_string();
            assert!(
                write_error.contains("single normal path component"),
                "{name:?}: {write_error}"
            );
            let local_error =
                write_local_qwen_inference(dir.path(), name, Path::new("/tmp/qwen.gguf"), 1, 1, 1)
                    .unwrap_err()
                    .to_string();
            assert!(
                local_error.contains("single normal path component"),
                "{name:?}: {local_error}"
            );
        }
        assert!(!dir.path().join("inferences").exists());
        assert!(!dir.path().join("x").exists());
    }

    #[test]
    fn writer_omits_name_and_emits_headers() {
        let dir = tempfile::tempdir().unwrap();
        write_inference(dir.path(), "p", "openai/gpt", 42).unwrap();
        let text =
            std::fs::read_to_string(dir.path().join("inferences/p/inference.textproto")).unwrap();
        assert!(text.starts_with(
            "# proto-file: proto/log.proto\n# proto-message: gait.log.v1.Inference\n"
        ));
        assert!(!text.contains("name:"));
    }
}
