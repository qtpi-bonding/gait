//! Extension load/unload commands. The session commands moved to the op
//! registry (`gait-bin::ops::session`).
use crate::cli::error::{CliError, CliResult};
use clap::Parser;
use std::path::PathBuf;

/// `gait load <kind> <name> --session <id>` — append a Toggle(enabled=true)
/// event to a session's log outside a running turn.
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct LoadArgs {
    /// Kind: mcp | skill | plugin
    pub kind: String,
    /// Extension name
    pub name: String,
    /// Session id
    pub session_id: String,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

/// `gait unload <kind> <name> --session <id>` — append a Toggle(enabled=false)
/// event to a session's log outside a running turn.
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct UnloadArgs {
    /// Kind: mcp | skill | plugin
    pub kind: String,
    /// Extension name
    pub name: String,
    /// Session id
    pub session_id: String,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

fn parse_ext_kind(kind: &str) -> Result<gait_ops::extension::ExtKind, String> {
    match kind {
        "mcp" => Ok(gait_ops::extension::ExtKind::Mcp),
        "skill" => Ok(gait_ops::extension::ExtKind::Skill),
        "plugin" => Ok(gait_ops::extension::ExtKind::Plugin),
        other => Err(format!(
            "unknown kind '{}': use mcp, skill, or plugin",
            other
        )),
    }
}

pub fn cmd_load(args: LoadArgs) -> CliResult {
    use gait_ops::extension::{load as op_load, LoadParams};

    let kind = match parse_ext_kind(&args.kind) {
        Ok(k) => k,
        Err(e) => return Err(CliError::invalid_input(format!("[gait] error: {}", e))),
    };
    match op_load(LoadParams {
        gait_dir: args.gait_dir,
        session_id: args.session_id.clone(),
        kind,
        name: args.name.clone(),
    }) {
        Ok(_) => {
            println!(
                "{}",
                gait_ops::render_json(
                    &serde_json::json!({
                        "session_id": args.session_id,
                        "kind": args.kind,
                        "name": args.name,
                        "action": "loaded",
                    }),
                    args.pretty
                )
            );
            Ok(())
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

pub fn cmd_unload(args: UnloadArgs) -> CliResult {
    use gait_ops::extension::{unload as op_unload, LoadParams};

    let kind = match parse_ext_kind(&args.kind) {
        Ok(k) => k,
        Err(e) => return Err(CliError::invalid_input(format!("[gait] error: {}", e))),
    };
    match op_unload(LoadParams {
        gait_dir: args.gait_dir,
        session_id: args.session_id.clone(),
        kind,
        name: args.name.clone(),
    }) {
        Ok(_) => {
            println!(
                "{}",
                gait_ops::render_json(
                    &serde_json::json!({
                        "session_id": args.session_id,
                        "kind": args.kind,
                        "name": args.name,
                        "action": "unloaded",
                    }),
                    args.pretty
                )
            );
            Ok(())
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::error::CliError;

    /// The typed variant a command failed with. Panics on an uncoded error,
    /// which `main` would otherwise report as exit 70 with no class.
    fn cli_error(err: &anyhow::Error) -> &CliError {
        coded_error::downcast_ref::<CliError>(err).expect("error is a CliError")
    }

    fn empty_gait_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }
    #[test]
    fn load_with_unknown_kind_is_invalid_input() {
        let dir = empty_gait_dir();
        let err = cmd_load(LoadArgs {
            kind: "widget".into(),
            name: "x".into(),
            session_id: "s1".into(),
            gait_dir: dir.path().to_path_buf(),
            pretty: false,
        })
        .unwrap_err();
        assert!(matches!(cli_error(&err), CliError::InvalidInput(_)));
        assert_eq!(
            format!("{err:#}"),
            "[gait] error: unknown kind 'widget': use mcp, skill, or plugin"
        );
    }

    #[test]
    fn unload_with_unknown_kind_is_invalid_input() {
        let dir = empty_gait_dir();
        let err = cmd_unload(UnloadArgs {
            kind: "widget".into(),
            name: "x".into(),
            session_id: "s1".into(),
            gait_dir: dir.path().to_path_buf(),
            pretty: false,
        })
        .unwrap_err();
        assert!(matches!(cli_error(&err), CliError::InvalidInput(_)));
    }

}
