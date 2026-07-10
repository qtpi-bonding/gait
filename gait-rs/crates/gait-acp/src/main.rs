//! gait-acp binary — delegates to the `gait_acp` library.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    gait_acp::run().await
}
