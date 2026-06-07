use serde::{Deserialize, Serialize};
use std::path::Path;

const MODELS_URL: &str = "https://models.dev/api.json";
const CACHE_TTL_SECS: u64 = 3600; // 1 hour

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub context_length: Option<u64>,
}

fn cache_path() -> std::path::PathBuf {
    dirs_next::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.cache"))
        .join("gait")
        .join("models.json")
}

pub fn load_from_cache(path: &Path) -> Option<Vec<ModelInfo>> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn cache_is_fresh(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    let Ok(modified) = meta.modified() else { return false };
    let Ok(age) = modified.elapsed() else { return false };
    age.as_secs() < CACHE_TTL_SECS
}

pub async fn get_models() -> anyhow::Result<Vec<ModelInfo>> {
    let cache = cache_path();

    if cache.exists() && cache_is_fresh(&cache) {
        if let Some(models) = load_from_cache(&cache) {
            return Ok(models);
        }
    }

    match fetch_models().await {
        Ok(models) => {
            if let Some(parent) = cache.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&cache, serde_json::to_string_pretty(&models)?);
            Ok(models)
        }
        Err(e) => {
            // Offline fallback: use stale cache
            if let Some(models) = load_from_cache(&cache) {
                eprintln!("[warn] models.dev unreachable, using cached model list");
                return Ok(models);
            }
            anyhow::bail!("Cannot fetch model list and no cache exists. Specify \"model\" directly in gait.jsonc. ({})", e)
        }
    }
}

async fn fetch_models() -> anyhow::Result<Vec<ModelInfo>> {
    let resp = reqwest::get(MODELS_URL).await?;
    if !resp.status().is_success() {
        anyhow::bail!("models.dev returned {}", resp.status());
    }
    let data: serde_json::Value = resp.json().await?;
    let arr = data.as_array()
        .ok_or_else(|| anyhow::anyhow!("Unexpected models.dev response shape"))?;
    Ok(arr.iter().filter_map(|v| {
        Some(ModelInfo {
            id: v.get("id")?.as_str()?.to_string(),
            name: v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
            provider: v.get("provider").and_then(|p| p.as_str()).unwrap_or("").to_string(),
            context_length: v.get("contextLength").and_then(|c| c.as_u64()),
        })
    }).collect())
}

pub fn filter_models<'a>(models: &'a [ModelInfo], filter: Option<&str>) -> Vec<&'a ModelInfo> {
    match filter {
        None => models.iter().collect(),
        Some(f) => {
            let f_lower = f.to_lowercase();
            models.iter().filter(|m|
                m.provider.to_lowercase().contains(&f_lower) ||
                m.id.to_lowercase().contains(&f_lower) ||
                m.name.to_lowercase().contains(&f_lower)
            ).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn get_models_from_cache() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("models.json");
        let data = serde_json::json!([
            { "id": "claude-3-5-sonnet", "name": "Claude 3.5 Sonnet", "provider": "anthropic" },
            { "id": "gpt-4o", "name": "GPT-4o", "provider": "openai" }
        ]);
        std::fs::write(&cache, serde_json::to_string(&data).unwrap()).unwrap();
        let models = load_from_cache(&cache).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "claude-3-5-sonnet");
        assert_eq!(models[0].provider, "anthropic");
    }

    #[test]
    fn filter_by_provider() {
        let models = vec![
            ModelInfo { id: "gpt-4o".into(), name: "GPT-4o".into(), provider: "openai".into(), context_length: None },
            ModelInfo { id: "claude-3-5-sonnet".into(), name: "Claude".into(), provider: "anthropic".into(), context_length: None },
        ];
        let filtered = filter_models(&models, Some("openai"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "gpt-4o");
    }

    #[test]
    fn filter_none_returns_all() {
        let models = vec![
            ModelInfo { id: "a".into(), name: "A".into(), provider: "pa".into(), context_length: None },
            ModelInfo { id: "b".into(), name: "B".into(), provider: "pb".into(), context_length: None },
        ];
        assert_eq!(filter_models(&models, None).len(), 2);
    }
}
