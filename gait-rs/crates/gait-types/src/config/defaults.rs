//! Single source of truth for every compiled-in default value.
//! `GaitConfig::default()`, the `gait init` scaffold, and the workflow-node
//! model resolution all derive from here so defaults can never drift.

use crate::config::{NetworkConfig, PreviewPolicy, SandboxConfig};

pub fn default_allow() -> Vec<String> {
    ["read", "glob", "grep", "ls", "clarify", "inform"]
        .into_iter()
        .map(String::from)
        .collect()
}

pub fn default_deny() -> Vec<String> {
    Vec::new()
}

pub fn default_compaction_threshold() -> f64 {
    0.8
}

pub fn default_output_schema_retries() -> u8 {
    3
}

pub fn default_max_iter() -> Option<usize> {
    None
}

pub fn default_preview() -> PreviewPolicy {
    PreviewPolicy::OnAsk
}

pub fn default_show_thinking() -> bool {
    false
}

pub fn default_sandbox() -> SandboxConfig {
    SandboxConfig {
        on: true,
        readable: Vec::new(),
        writable: Vec::new(),
        network: NetworkConfig::Off,
        shell_whitelist: Vec::new(),
        tools: std::collections::HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_allow_is_read_only_set() {
        assert_eq!(
            default_allow(),
            vec!["read", "glob", "grep", "ls", "clarify", "inform"]
        );
    }

    #[test]
    fn default_sandbox_is_on_network_off() {
        let s = default_sandbox();
        assert!(s.on);
        assert!(s.writable.is_empty());
        assert!(matches!(s.network, crate::config::NetworkConfig::Off));
    }
}
