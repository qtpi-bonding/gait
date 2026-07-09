//! `gait link <gait-dir>` — write a `.gaitref` pointer in cwd naming an existing
//! gait workspace, so `gait_core::gait_dir::resolve` finds it by walk-up.
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
}

pub fn cmd_link(args: LinkArgs) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let target = if args.gait_dir.is_absolute() {
        args.gait_dir.clone()
    } else {
        cwd.join(&args.gait_dir)
    };
    if !target.join(".git").exists() {
        eprintln!(
            "[gait] {} is not a gait workspace (no .git) — run `gait init {}` first",
            target.display(),
            target.display()
        );
        std::process::exit(1);
    }
    if let Err(e) = std::fs::write(cwd.join(".gaitref"), pointer_body(&target)) {
        eprintln!("[gait] cannot write .gaitref: {e}");
        std::process::exit(1);
    }
    println!(
        "[gait] linked {} -> {}",
        cwd.join(".gaitref").display(),
        target.display()
    );
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
