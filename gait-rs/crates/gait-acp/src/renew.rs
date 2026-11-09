//! Keeps the process's device identity current: renews a folder gait minted
//! before its delegation lapses, and rebuilds the process verifier's device
//! snapshot whenever the folder may have changed (our own renewal, or an
//! outside renewal of a handed folder). Roles are never restarted; each turn's
//! role processes load the folder fresh.
use std::sync::Arc;
use std::time::Duration;

use gait_identity::{DirectVerifier, IdentityContext};

const CHECK_EVERY: Duration = Duration::from_secs(3600);

/// One pass: renew what gait minted (if due), then refresh the verifier.
pub fn tick(ctx: &IdentityContext, verifier: &DirectVerifier, now: i64) {
    let _span = tracing::info_span!("gait.identity.renew_tick").entered();
    if let Ok(layout) = split_layout_of(ctx) {
        if let Err(error) = gait_identity::renew_minted(&layout, now) {
            concord_telemetry::error_anyhow!(&error, source = "gait-acp.renew");
        }
    }
    if let Err(error) = verifier.refresh(ctx) {
        concord_telemetry::error_anyhow!(&error, source = "gait-acp.renew");
    }
}

/// The minted layout, when `ctx` is the `<state>/identity` folder next to a
/// sibling `identity-root`; otherwise an error (device-only folder: no renewal).
fn split_layout_of(ctx: &IdentityContext) -> anyhow::Result<identity_keyfile::IdentityLayout> {
    let device = ctx.agent_dir();
    let parent = device.parent().ok_or_else(|| anyhow::anyhow!("no parent"))?;
    if device.file_name().and_then(|n| n.to_str()) != Some("identity") {
        anyhow::bail!("not a minted folder");
    }
    Ok(identity_keyfile::IdentityLayout::split(&parent.join("identity-root"), device)?)
}

pub fn spawn(ctx: Arc<IdentityContext>, verifier: Arc<DirectVerifier>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CHECK_EVERY).await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            tick(&ctx, &verifier, now);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_renews_a_minted_folder_and_refreshes_the_verifier() {
        let gait = tempfile::tempdir().unwrap();
        let ctx = IdentityContext::mint_or_load(gait.path()).unwrap();
        let verifier = DirectVerifier::from_context(&ctx).unwrap();
        let later = gait_identity::DELEGATION_TTL_SECS;
        tick(&ctx, &verifier, later);
        let layout = split_layout_of(&ctx).unwrap();
        assert!(!gait_identity::renew_minted(&layout, later).unwrap(), "already renewed");
    }

    #[test]
    fn a_handed_device_only_folder_has_no_minted_layout() {
        let dir = tempfile::tempdir().unwrap();
        let handed = dir.path().join("agent-x");
        std::fs::create_dir_all(&handed).unwrap();
        let keys = identity_keyfile::testkit::TempIdentityDir::new(0, 86_400);
        let ctx = IdentityContext::new(keys.path()).unwrap();
        assert!(split_layout_of(&ctx).is_err() || !handed.join("root.jwk.json").exists());
    }
}
