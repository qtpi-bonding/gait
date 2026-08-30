//! `gait link <gait-dir>` — write a `.gaitref` pointer in cwd naming an existing
//! gait workspace, so `gait_core::gait_dir::resolve` finds it by walk-up.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

use std::path::{Path, PathBuf};

pub fn pointer_body(gait_dir: &Path) -> String {
    format!("gaitdir: {}\n", gait_dir.display())
}

/// clap args for `gait link`. Mirror `InitArgs`'s derive exactly
/// (`#[derive(Parser)]` + `#[command(no_binary_name = true)]`, per init.rs:6-11).
#[derive(clap::Parser, Debug)]
#[command(no_binary_name = true)]
pub struct LinkArgs {
    /// Path to an existing gait workspace to point at.
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

pub fn cmd_link(args: LinkArgs) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let target = if args.gait_dir.is_absolute() {
        args.gait_dir.clone()
    } else {
        cwd.join(&args.gait_dir)
    };
    match gait_ops::workspace::link(&cwd, &target) {
        Ok(out) => println!(
            "{}",
            gait_ops::render_json(
                &serde_json::json!({
                    "gaitref": out.gaitref.display().to_string(),
                    "target": out.target.display().to_string(),
                }),
                args.pretty
            )
        ),
        Err(e) => {
            eprintln!("[gait] {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn pointer_body_prefixed_absolute() {
        assert_eq!(
            pointer_body(&PathBuf::from("/opt/home")),
            "gaitdir: /opt/home\n"
        );
    }

    #[test]
    #[serial_test::serial]
    fn round_trips_through_core_resolver() {
        // `resolve(None, ...)` honors $GAIT_DIR before walk-up (by design), so a
        // dev shell with $GAIT_DIR exported would otherwise short-circuit this
        // test before it ever reaches the .gaitref walk-up being verified here.
        let saved = std::env::var("GAIT_DIR").ok();
        std::env::remove_var("GAIT_DIR");
        let t = tempfile::TempDir::new().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let gait_dir = t.path().join("home");
        std::fs::write(repo.join(".gaitref"), pointer_body(&gait_dir)).unwrap();
        let result = gait_core::gait_dir::resolve(None, &repo);
        if let Some(v) = saved {
            std::env::set_var("GAIT_DIR", v);
        }
        assert_eq!(result.unwrap(), gait_dir);
    }
}
