//! Hidden `gait __run-tool` — the ephemeral "hand".
//!
//! Thin CLI wrapper around `gait_core::turn::hand::run_hand`. The hand runtime
//! (seal application, scope construction, tool execution, Feedback encoding)
//! lives in `gait-core/src/turn/hand.rs`. This file parses args and delegates.
use clap::Parser;
use std::path::PathBuf;

use gait_core::turn::hand::{run_hand, HandConfig};

#[derive(Parser)]
#[command(no_binary_name = true)]
struct RunToolArgs {
    #[arg(long)]
    gait_dir: PathBuf,
    #[arg(long)]
    codebase: PathBuf,
    #[arg(long)]
    session: String,
    #[arg(long)]
    tool: String,
    /// When false, skip platform_sandbox().apply() and use NoopSandbox instead.
    /// Set by the spine based on the per-turn sandbox engagement flag.
    #[arg(long, default_value = "true")]
    seal: bool,
}

pub async fn cmd_run_tool(argv: &[String]) {
    let a = RunToolArgs::parse_from(argv);

    let config = HandConfig {
        gait_dir: a.gait_dir,
        codebase: a.codebase,
        session: a.session,
        tool: a.tool,
        seal: a.seal,
    };

    run_hand(config).await;
}
