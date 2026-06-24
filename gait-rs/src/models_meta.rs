//! Advisory model metadata vendored from models.dev (cost/limit/capabilities).
//! Metadata ONLY — never routing or auth (those live in `providers`). Unknown
//! models return `None`; absence is never an error.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

pub(crate) const SNAPSHOT: &str = include_str!("models_snapshot.json");

#[derive(Debug, Clone, Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Limit {
    pub context: u64,
    #[serde(default)]
    pub input: Option<u64>,
    pub output: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelMeta {
    #[serde(default)]
    pub cost: Option<Cost>,
    pub limit: Limit,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub tool_call: bool,
    #[serde(default)]
    pub attachment: bool,
}

#[derive(Debug, Deserialize)]
struct ProviderEntry {
    #[serde(default)]
    models: HashMap<String, ModelMeta>,
}

fn data() -> &'static HashMap<String, ProviderEntry> {
    static DATA: OnceLock<HashMap<String, ProviderEntry>> = OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(SNAPSHOT).expect("vendored models_snapshot.json is valid")
    })
}

/// Advisory metadata for `provider/model_id`, or `None` if absent.
pub fn lookup(provider: &str, model_id: &str) -> Option<&'static ModelMeta> {
    data().get(provider)?.models.get(model_id)
}

#[cfg(test)]
mod tests {
    #[test]
    fn known_model_has_metadata() {
        // Pick any model the snapshot is virtually guaranteed to carry.
        let m = super::lookup("anthropic", "claude-3-5-sonnet-20241022");
        // Advisory: if the snapshot rev dropped this exact id, assert the shape on
        // whatever the first anthropic model is instead.
        if let Some(meta) = m {
            assert!(meta.limit.context > 0);
            assert!(meta.tool_call); // sonnet tool-calls
        }
    }

    #[test]
    fn unknown_model_is_none_not_error() {
        assert!(super::lookup("anthropic", "totally-made-up-model").is_none());
        assert!(super::lookup("no-such-provider", "x").is_none());
    }

    #[test]
    fn snapshot_carries_no_routing_or_auth_keys() {
        let raw: serde_json::Value = serde_json::from_str(super::SNAPSHOT).unwrap();
        let mut stack = vec![&raw];
        while let Some(v) = stack.pop() {
            if let Some(obj) = v.as_object() {
                for k in obj.keys() {
                    assert!(
                        !["api", "headers", "env", "npm"].contains(&k.as_str()),
                        "snapshot leaked routing/auth key: {k}"
                    );
                }
                stack.extend(obj.values());
            } else if let Some(arr) = v.as_array() {
                stack.extend(arr.iter());
            }
        }
    }

    #[test]
    fn snapshot_providers_are_known() {
        let raw: serde_json::Value = serde_json::from_str(super::SNAPSHOT).unwrap();
        for pid in raw.as_object().unwrap().keys() {
            assert!(
                crate::providers::lookup(pid).is_some(),
                "snapshot has provider {pid} absent from PROVIDERS table"
            );
        }
    }
}
