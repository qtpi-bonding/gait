use std::path::Path;

/// The three kinds of sandbox grant a user can persist — the single vocabulary
/// for the sandbox-grant WRITE-side: CLI approval → this enum → the gait.jsonc
/// `sandbox` block. It is READ back on the enforcement side by
/// `gait_core::turn::brain` (cfg.sandbox → Impulse ceiling) and applied at the
/// kernel by `gait_core::turn::hand::build_scope_from_impulse` +
/// `gait_sandbox::sandbox`. Those steps are the brain/hand seal and are NOT
/// owned here (see spec-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxGrant {
    Writable,
    Readable,
    NetworkHost,
}

impl SandboxGrant {
    pub fn from_dimension(s: &str) -> Option<Self> {
        match s {
            "write" => Some(Self::Writable),
            "read" => Some(Self::Readable),
            "network" => Some(Self::NetworkHost),
            _ => None,
        }
    }

    pub fn apply(self, target_dir: &Path, resource: &str) -> anyhow::Result<()> {
        match self {
            Self::Writable => crate::config::append_sandbox_writable(target_dir, resource),
            Self::Readable => crate::config::append_sandbox_readable(target_dir, resource),
            Self::NetworkHost => crate::config::append_sandbox_network_host(target_dir, resource),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // `load_config` is infallible and returns the full GaitConfig (verified:
    // crates/gait-types/src/config/mod.rs:494 `pub fn load_config(gait_dir: &Path) -> GaitConfig`).
    fn load_sandbox(dir: &Path) -> crate::config::SandboxConfig {
        crate::config::load_config(dir).sandbox
    }

    #[test]
    fn from_dimension_maps_the_three_kinds() {
        assert_eq!(
            SandboxGrant::from_dimension("write"),
            Some(SandboxGrant::Writable)
        );
        assert_eq!(
            SandboxGrant::from_dimension("read"),
            Some(SandboxGrant::Readable)
        );
        assert_eq!(
            SandboxGrant::from_dimension("network"),
            Some(SandboxGrant::NetworkHost)
        );
        assert_eq!(SandboxGrant::from_dimension("bogus"), None);
    }

    #[test]
    fn apply_writable_persists_to_config() {
        let tmp = tempfile::tempdir().unwrap();
        SandboxGrant::Writable.apply(tmp.path(), "src/").unwrap();
        let sb = load_sandbox(tmp.path());
        // writable is Vec<String> (config/mod.rs:177); &String == &str compares fine.
        assert!(
            sb.writable.iter().any(|p| p == "src/"),
            "applied writable grant must appear in sandbox.writable"
        );
    }

    #[test]
    fn apply_readable_and_network_persist_to_config() {
        let tmp = tempfile::tempdir().unwrap();
        SandboxGrant::Readable.apply(tmp.path(), "docs/").unwrap();
        SandboxGrant::NetworkHost
            .apply(tmp.path(), "api.example.com")
            .unwrap();
        let sb = load_sandbox(tmp.path());
        assert!(sb.readable.iter().any(|p| p == "docs/"));
        // network is enum NetworkConfig::{Off, All, Hosts(Vec<String>)} (config/mod.rs).
        assert!(
            matches!(&sb.network, crate::config::NetworkConfig::Hosts(h)
                if h.iter().any(|x| x == "api.example.com")),
            "network-host grant must land in NetworkConfig::Hosts, got {:?}",
            sb.network
        );
    }
}
