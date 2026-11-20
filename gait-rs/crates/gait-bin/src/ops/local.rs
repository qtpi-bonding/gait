//! Local-model ops: set up a llama.cpp profile and manage its runner. Only
//! built with the `local` feature; declared in its own `ops!` block and merged
//! into the registry.
use super::{infra, GaitContext};
use gait_cli::cli::error::CliError;
use op_registry::{CallOptions, OpParams};
use std::path::Path;

output! {
    /// A local inference profile that was written.
    pub struct LocalSetup {
        pub name: String,
        pub model: String,
        /// The GGUF file the profile points at.
        pub gguf: String,
        /// Always `added`.
        pub action: String,
    }
}

output! {
    /// The state of a local model runner.
    pub struct LocalRunner {
        /// `running`, `stopped` or `not running`.
        pub status: String,
        /// The loaded model's key; only while running.
        pub model_key: Option<String>,
    }
}

#[derive(Debug, Clone, OpParams)]
pub struct LocalSetupParams {
    /// The profile name; `local-qwen` when not given.
    pub name: Option<String>,
    /// The GGUF file of Qwen2.5-Coder-7B-Instruct Q4_K_M.
    pub model_path: String,
    /// The context window to give llama.cpp; 8192 is a conservative starter.
    #[op(default = 8192)]
    pub context_tokens: u32,
    /// The llama.cpp layers to offload; 0 is CPU-only.
    #[op(default = 0)]
    pub gpu_layers: u32,
    /// Begin compaction at this many tokens, before the context is full.
    #[op(default = 6000)]
    pub compaction_threshold_tokens: u64,
}

#[derive(Debug, Clone, OpParams)]
pub struct LocalRunnerParams {
    /// The profile name; `local-qwen` when not given.
    pub name: Option<String>,
}

const DEFAULT_NAME: &str = "local-qwen";

/// Write a local inference profile for a Qwen2.5-Coder GGUF file.
pub async fn setup(
    ctx: &GaitContext,
    params: LocalSetupParams,
    _opts: CallOptions,
) -> anyhow::Result<LocalSetup> {
    let name = params.name.as_deref().unwrap_or(DEFAULT_NAME);
    let info = gait_ops::inference::add_local_qwen(
        &ctx.gait_dir()?,
        name,
        Path::new(&params.model_path),
        params.context_tokens,
        params.gpu_layers,
        params.compaction_threshold_tokens,
    )
    .map_err(infra)?;
    Ok(LocalSetup {
        name: info.name,
        model: info.model,
        gguf: params.model_path,
        action: "added".into(),
    })
}

async fn runner(
    ctx: &GaitContext,
    params: LocalRunnerParams,
    action: &str,
) -> anyhow::Result<LocalRunner> {
    let dir = ctx.gait_dir()?;
    let name = params.name.as_deref().unwrap_or(DEFAULT_NAME);
    let inference = gait_core::inferences::load_inference(&dir, name).map_err(infra)?;
    let not_local = || CliError::invalid_input(format!("inference '{name}' is not a configured local profile"));
    let llm = inference
        .llm_params
        .as_ref()
        .map(gait_core::proto::events::llm_params_from_proto)
        .transpose()
        .ok()
        .flatten()
        .ok_or_else(not_local)?;
    let local = llm.local.ok_or_else(not_local)?;
    let protocol = if inference.model == "local/qwen2.5-coder-7b-instruct-q4-k-m" {
        "qwen25-coder"
    } else {
        "unsupported"
    };
    let key = gait_local::runner_key(&format!(
        "model={};ctx={};gpu={:?};tool={protocol}",
        local.model_path,
        local.context_tokens.unwrap_or(32_768),
        local.gpu_layers.or(Some(0))
    ));
    let socket = dir.join("local-runners").join(format!("{key}.sock"));
    let state = |status: &str, model_key: Option<String>| LocalRunner { status: status.into(), model_key };
    match runner_command(&socket, action).await {
        Ok(Some(gait_local::RunnerResponse::Status { model_key })) => Ok(state("running", Some(model_key))),
        Ok(Some(gait_local::RunnerResponse::Stopped)) => Ok(state("stopped", None)),
        Ok(_) => Ok(state("not running", None)),
        Err(_) if action == "status" => Ok(state("not running", None)),
        Err(error) => Err(CliError::failed(format!("local runner: {error}"))),
    }
}

async fn runner_command(
    socket: &Path,
    action: &str,
) -> Result<Option<gait_local::RunnerResponse>, std::io::Error> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let request = if action == "stop" {
        gait_local::RunnerRequest::Stop
    } else {
        gait_local::RunnerRequest::Status
    };
    stream
        .write_all(serde_json::to_string(&request).expect("runner request serializes").as_bytes())
        .await?;
    stream.write_all(b"\n").await?;
    let mut lines = tokio::io::BufReader::new(stream).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(None);
    };
    Ok(serde_json::from_str(&line).ok())
}

/// Show whether the local model runner for a profile is running.
pub async fn status(
    ctx: &GaitContext,
    params: LocalRunnerParams,
    _opts: CallOptions,
) -> anyhow::Result<LocalRunner> {
    runner(ctx, params, "status").await
}

/// Stop the local model runner for a profile.
pub async fn stop(
    ctx: &GaitContext,
    params: LocalRunnerParams,
    _opts: CallOptions,
) -> anyhow::Result<LocalRunner> {
    runner(ctx, params, "stop").await
}

/// The local-model ops, merged into the registry when the feature is on.
pub mod decl {
    use super::{LocalRunner, LocalRunnerParams, LocalSetup, LocalSetupParams};
    use crate::ops::GaitContext;
    use crate::ops::Globals;
    use gait_cli::cli::error::CliError;
    use gait_ops::OpError;

    op_registry::ops! {
        Ctx = GaitContext;
        globals = Globals;
        pillar = "gait";
        bin = "gait"; version = env!("CARGO_PKG_VERSION"); about = "AI coding agent";
        groups = {
            "local" => "Local models run by llama.cpp: set up a profile and manage its runner.",
        };
        /// Write a local inference profile for a Qwen2.5-Coder GGUF file.
        "local setup" => { params: LocalSetupParams, output: LocalSetup,
                           access: writes, audience: owner, op: super::setup,
                           errors: [OpError, CliError] },
        /// Show whether the local model runner for a profile is running.
        "local status" => { params: LocalRunnerParams, output: LocalRunner,
                            access: reads, audience: owner, op: super::status,
                            errors: [OpError, CliError] },
        /// Stop the local model runner for a profile.
        "local stop" => { params: LocalRunnerParams, output: LocalRunner,
                          access: writes, audience: owner, op: super::stop,
                          errors: [OpError, CliError] },
    }
}
