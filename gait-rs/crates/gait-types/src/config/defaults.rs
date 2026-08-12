//! Single source of truth for every compiled-in default value.
//! `GaitConfig::default()`, the `gait init` scaffold, and the workflow-node
//! model resolution all derive from here so defaults can never drift.

use crate::config::{DenyConfig, NetworkConfig, PreviewPolicy, SandboxConfig};

pub fn default_allow() -> Vec<String> {
    ["read", "glob", "grep", "ls", "clarify", "inform"]
        .into_iter()
        .map(String::from)
        .collect()
}

pub fn default_deny() -> Vec<String> {
    Vec::new()
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

/// Validated built-in secrets denylist — read AND written by the `gait init`
/// scaffold as real, editable JSON, not hidden. See spec §2a for the
/// per-pattern validation reasoning (`.git/config` deliberately absent,
/// `**/id_*` narrowed to real SSH key filenames).
pub fn default_deny_patterns() -> Vec<String> {
    [
        "**/.env*",
        "**/id_rsa",
        "**/id_dsa",
        "**/id_ecdsa",
        "**/id_ed25519",
        "~/.ssh/**",
        "~/.aws/**",
        "~/.config/gh/**",
        // macOS keychain — user's login keychain database file.
        "~/Library/Keychains/**",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

pub fn default_sandbox() -> SandboxConfig {
    SandboxConfig {
        on: true,
        readable: Vec::new(),
        writable: Vec::new(),
        network: NetworkConfig::Off,
        shell_whitelist: Vec::new(),
        tools: std::collections::HashMap::new(),
        deny: DenyConfig::default(),
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

    #[test]
    fn default_deny_patterns_excludes_dot_git_config_and_broad_id_star() {
        let patterns = default_deny_patterns();
        // .git/config is deliberately held back — denying it kernel-wide breaks
        // ordinary `git status`/`git log`/`git commit` run inside the sandbox
        // (git's own subprocess reads its own config). See spec §2a.
        assert!(!patterns.iter().any(|p| p.contains(".git/config")));
        // A bare "**/id_*" glob false-positives on ordinary source files like
        // id_map.rs — must be narrowed to actual SSH private-key filenames.
        assert!(!patterns.contains(&"**/id_*".to_string()));
        assert!(patterns.contains(&"**/id_rsa".to_string()));
        assert!(patterns.contains(&"**/id_ed25519".to_string()));
    }

    #[test]
    fn default_deny_patterns_covers_env_and_credential_dirs() {
        let patterns = default_deny_patterns();
        assert!(patterns.contains(&"**/.env*".to_string()));
        assert!(patterns.iter().any(|p| p.contains(".ssh")));
        assert!(patterns.iter().any(|p| p.contains(".aws")));
        assert!(patterns.iter().any(|p| p.contains(".config/gh")));
    }
}
