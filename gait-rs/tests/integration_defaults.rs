// Proves: with no config file, every consumer-shaped resolution agrees on the
// same defaults, and an unset model errors rather than silently defaulting.
use gait_core::config::{defaults, load_config, ConfigResolver, Overrides};

#[test]
fn all_consumers_share_one_default_source() {
    let dir = tempfile::tempdir().unwrap(); // no gait.jsonc
    let base = load_config(dir.path());
    assert_eq!(base.allow, defaults::default_allow());
    assert_eq!(base.inference, None);

    // consumer with no overrides must still equal the source-of-truth allow-set
    let effective = ConfigResolver::new(base).with(Overrides::empty()).resolve();
    assert_eq!(effective.allow, defaults::default_allow());
}

#[test]
fn config_less_session_requires_explicit_model() {
    let dir = tempfile::tempdir().unwrap();
    let base = load_config(dir.path());
    assert!(ConfigResolver::new(base).resolve_checked().is_err());
}
