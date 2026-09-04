//! gait-acp binary — delegates to the `gait_acp` library. Also answers to the
//! same sealed role tokens (`__brain`, `__run-tool`, `__daemon-exit`) that the
//! `gait` binary does: `gait_turn_runner`'s brain-launch path spawns
//! `current_exe() __brain ...`, and when a turn is driven from inside
//! `gait-acp` (not `gait`), `current_exe()` resolves to this binary. Without
//! this pre-check, that spawn would produce a `gait-acp` child with no role
//! dispatch at all, which just reran `gait_acp::run()` and misread the
//! brain's binary Nerve init frame as malformed ACP JSON-RPC.
//!
//! `--require-attestation` and (repeatable) `--trusted-multimatrix-key
//! <key_id>:<hex-pubkey>` are the process-level attestation policy --
//! deliberately parsed here, not read from any runtime `_meta` claim, since
//! the trust boundary has to be rooted in how the process itself was
//! launched (e.g. by `multimatrix-bridge`). Unrecognized/malformed argv is
//! left alone (no clap dependency here today) -- only these two flags are
//! consumed; anything else is ignored, matching this binary's existing
//! no-other-CLI-surface behavior.
fn parse_attestation_flags(
    argv: &[String],
) -> (bool, Vec<gait_ops::attestation::TrustedMultimatrixKey>) {
    let mut require_attestation = false;
    let mut trusted_keys = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--require-attestation" => require_attestation = true,
            "--trusted-multimatrix-key" => {
                if let Some(value) = argv.get(i + 1) {
                    if let Some((key_id, hex_pubkey)) = value.split_once(':') {
                        trusted_keys.push(gait_ops::attestation::TrustedMultimatrixKey {
                            key_id: key_id.to_string(),
                            public_key_hex: hex_pubkey.to_string(),
                        });
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    (require_attestation, trusted_keys)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() >= 2 && gait_core::is_role_argv(&raw[1..]) {
        gait_core::__dispatch_role(&raw[1..]).await;
        return Ok(());
    }
    let (require_attestation, trusted_keys) = parse_attestation_flags(&raw[1..]);
    gait_acp::run_with_attestation(require_attestation, trusted_keys).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_require_attestation_flag() {
        let argv = vec!["--require-attestation".to_string()];
        let (require, keys) = parse_attestation_flags(&argv);
        assert!(require);
        assert!(keys.is_empty());
    }

    #[test]
    fn parses_repeated_trusted_multimatrix_keys() {
        let argv = vec![
            "--trusted-multimatrix-key".to_string(),
            "mm-key-1:deadbeef".to_string(),
            "--trusted-multimatrix-key".to_string(),
            "mm-key-2:cafef00d".to_string(),
        ];
        let (require, keys) = parse_attestation_flags(&argv);
        assert!(!require);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key_id, "mm-key-1");
        assert_eq!(keys[0].public_key_hex, "deadbeef");
        assert_eq!(keys[1].key_id, "mm-key-2");
        assert_eq!(keys[1].public_key_hex, "cafef00d");
    }

    #[test]
    fn no_attestation_flags_yields_permissive_defaults() {
        let argv: Vec<String> = vec![];
        let (require, keys) = parse_attestation_flags(&argv);
        assert!(!require);
        assert!(keys.is_empty());
    }
}
