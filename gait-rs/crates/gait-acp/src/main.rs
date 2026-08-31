//! gait-acp binary — delegates to the `gait_acp` library. Also answers to the
//! same sealed role tokens (`__brain`, `__run-tool`, `__daemon-exit`) that the
//! `gait` binary does: `gait_workflow`'s brain-launch path spawns
//! `current_exe() __brain ...`, and when a turn is driven from inside
//! `gait-acp` (not `gait`), `current_exe()` resolves to this binary. Without
//! this pre-check, that spawn would produce a `gait-acp` child with no role
//! dispatch at all, which just reran `gait_acp::run()` and misread the
//! brain's binary Nerve init frame as malformed ACP JSON-RPC.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() >= 2 && gait_core::is_role_argv(&raw[1..]) {
        gait_core::__dispatch_role(&raw[1..]).await;
        return Ok(());
    }
    gait_acp::run().await
}
