use super::GaitConfig;

/// A layer of per-call or consumer overrides.
/// Each field is `None` = "this layer doesn't set it"; `Some(v)` = "this layer sets it to v".
/// `max_iter` uses `Option<Option<u32>>`: outer `Some` means "this layer sets it",
/// inner `None` means "set it to unlimited".
pub struct Overrides {
    pub model: Option<String>,
    pub allow: Option<Vec<String>>,
    pub deny: Option<Vec<String>>,
    pub max_iter: Option<Option<u32>>,
    pub agent: Option<String>,
}

impl Overrides {
    /// Returns an `Overrides` with all fields set to `None` (no overrides).
    pub fn empty() -> Self {
        Self {
            model: None,
            allow: None,
            deny: None,
            max_iter: None,
            agent: None,
        }
    }
}

/// Four-layer config resolver: Defaults < gait.jsonc < consumer < per-call.
///
/// Usage:
/// ```ignore
/// let effective = ConfigResolver::new(loaded_config)
///     .with(consumer_overrides)
///     .with(per_call_overrides)
///     .resolve();
/// ```
pub struct ConfigResolver {
    base: GaitConfig,
}

impl ConfigResolver {
    /// Create a resolver seeded with the already-merged base config
    /// (i.e., `load_config(gait_dir)` which already folds Defaults < gait.jsonc).
    pub fn new(base: GaitConfig) -> Self {
        Self { base }
    }

    /// Apply a layer on top of the current base. Fields that are `Some(v)` in
    /// `layer` overwrite the base; `None` fields are left unchanged.
    /// Call in order from lower-priority to higher-priority; last `with` wins.
    pub fn with(mut self, layer: Overrides) -> Self {
        if let Some(model) = layer.model {
            self.base.model = model;
        }
        if let Some(allow) = layer.allow {
            self.base.allow = allow;
        }
        if let Some(deny) = layer.deny {
            self.base.deny = deny;
        }
        if let Some(max_iter) = layer.max_iter {
            // max_iter in GaitConfig is Option<usize>; we store as Option<u32> in Overrides
            self.base.max_iter = max_iter.map(|v| v as usize);
        }
        // `agent` is not a field on GaitConfig; it's used by callers to route agent resolution.
        // We hold it in the Overrides struct for future use but don't apply it to GaitConfig here.
        let _ = layer.agent;
        self
    }

    /// Collapse the accumulated layers into a final `GaitConfig`.
    pub fn resolve(self) -> GaitConfig {
        self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GaitConfig;

    #[test]
    fn per_call_beats_consumer_beats_file() {
        let mut base = GaitConfig::default();
        base.model = "file-model".into(); // stands in for the gait.jsonc layer
        let effective = ConfigResolver::new(base)
            .with(Overrides {
                model: Some("consumer-model".into()),
                ..Overrides::empty()
            })
            .with(Overrides {
                model: Some("percall-model".into()),
                ..Overrides::empty()
            })
            .resolve();
        assert_eq!(effective.model, "percall-model");
    }

    #[test]
    fn empty_overrides_preserve_base() {
        let base = GaitConfig::default();
        let effective = ConfigResolver::new(base.clone())
            .with(Overrides::empty())
            .resolve();
        assert_eq!(effective.allow, base.allow);
    }
}
