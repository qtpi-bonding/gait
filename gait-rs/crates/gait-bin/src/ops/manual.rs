//! Manual ops of the full `gait`: `manual` prints the generated manual, and
//! `manual install` writes it into the gait directory as the `gait-manual`
//! skill. The command help in it comes from the registry itself.
use super::{infra, GaitContext};
use crate::manual::{install_text, render, ManualFlavor};
use op_registry::manual::ManualOutput;
use op_registry::{CallOptions, OpParams};

output! {
    /// Where the manual skill was written.
    pub struct ManualInstalled {
        /// The `SKILL.md` that was written.
        pub installed: String,
    }
}

#[derive(Debug, Clone, OpParams)]
pub struct ManualShowParams {
    /// Return every operation's descriptor, schema, exit codes and error codes as JSON instead of the Markdown manual.
    pub json: bool,
}

#[derive(Debug, Clone, OpParams)]
pub struct ManualInstallParams {
    /// Replace an existing generated manual.
    pub force: bool,
}

fn manual_error(error: crate::manual::ManualError) -> anyhow::Error {
    infra(anyhow::Error::from(error))
}

/// Print the manual: guides, reference and every command, as Markdown, or with `--json` as descriptors.
pub async fn show(
    _ctx: &GaitContext,
    params: ManualShowParams,
    _opts: CallOptions,
) -> anyhow::Result<ManualOutput> {
    let registry = super::registry();
    if params.json {
        return Ok(ManualOutput::Json(op_registry::manual::render_json(&registry)));
    }
    let markdown = render(&registry, ManualFlavor::Full, gait_repl::repl::commands::COMMANDS)
        .map_err(manual_error)?;
    Ok(ManualOutput::Markdown { markdown })
}

/// Write the manual to `skills/gait-manual/SKILL.md` in the gait directory.
pub async fn install(
    ctx: &GaitContext,
    params: ManualInstallParams,
    _opts: CallOptions,
) -> anyhow::Result<ManualInstalled> {
    let gait_dir = ctx.gait_dir()?;
    let path = gait_dir.join("skills/gait-manual/SKILL.md");
    if path.exists() && !params.force {
        return Err(coded_error::into_anyhow(gait_ops::OpError::Conflict(format!(
            "{} already exists; pass --force to regenerate it",
            path.display()
        ))));
    }
    let content = install_text(&super::registry(), ManualFlavor::Full, gait_repl::repl::commands::COMMANDS)
        .map_err(manual_error)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(infra)?;
    }
    std::fs::write(&path, content).map_err(infra)?;
    Ok(ManualInstalled { installed: path.display().to_string() })
}

/// The manual ops of the full `gait`, merged into the registry.
pub mod decl {
    use super::{ManualInstallParams, ManualInstalled, ManualShowParams};
    use crate::ops::{GaitContext, Globals};
    use gait_cli::cli::error::CliError;
    use gait_ops::OpError;
    use op_registry::manual::ManualOutput;

    op_registry::ops! {
        Ctx = GaitContext;
        globals = Globals;
        pillar = "gait";
        bin = "gait"; version = env!("CARGO_PKG_VERSION"); about = "AI coding agent";
        groups = {
            "manual" => "The generated manual of every command, and its install as an agent skill.",
        };
        /// Print the manual: guides, reference and every command, as Markdown, or with `--json` as descriptors.
        "manual show" => { params: ManualShowParams, output: ManualOutput,
                           access: reads, audience: owner, local: super::show,
                           errors: [CliError], default: true, text: true },
        /// Write the manual to `skills/gait-manual/SKILL.md` in the gait directory, for agents to read.
        "manual install" => { params: ManualInstallParams, output: ManualInstalled,
                              access: writes, audience: owner, local: super::install,
                              errors: [OpError, CliError] },
    }
}
