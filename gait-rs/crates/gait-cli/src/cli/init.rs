//! `gait init` — scaffold a gait-dir and make an initial commit.

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct InitArgs {
    /// Target gait-dir (default: current directory).
    pub path: Option<PathBuf>,
}

fn print_init_summary(gait_dir: &std::path::Path) {
    let display = gait_dir.display();
    eprintln!("[gait] initialized gait-dir at {display}");
    eprintln!();
    eprintln!("  {:<32} {:<46} git", "file", "purpose");
    eprintln!(
        "  {:<32} {:<46} committed",
        "gait.jsonc", "model + tool-permission config"
    );
    eprintln!(
        "  {:<32} {:<46} committed",
        "agents/default/AGENT.md", "the agent's role and instructions"
    );
    eprintln!(
        "  {:<32} {:<46} ignored",
        "USER.md", "your personal instructions (not shared)"
    );
    eprintln!(
        "  {:<32} {:<46} committed*",
        "skills/", "installed skills  (skills/<n>/SKILL.md)"
    );
    eprintln!("  {:<32} {:<46} ignored", "sessions/", "turn history");
    eprintln!();
    eprintln!("  * empty dir; git-tracked when you add skills");
    eprintln!();
    eprintln!("  next: gait run --task \"...\"");
}

/// Thin wrapper around [`scaffold_gait_dir`] for use in tests and programmatic callers
/// that want to initialize a gait-dir without the git init / staging / commit steps.
pub fn run_init(gait_dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(gait_dir)?;
    scaffold_gait_dir(gait_dir)
}

/// Create the empty, editable layer affordances in the gait-dir (idempotent,
/// never clobbers) and ensure USER.md and sessions/ are gitignored.
pub fn scaffold_gait_dir(gait_dir: &std::path::Path) -> std::io::Result<()> {
    // agents/default/ — Phase 4 directory form
    let default_agent_dir = gait_dir.join("agents").join("default");
    std::fs::create_dir_all(&default_agent_dir)?;
    let default_agent = default_agent_dir.join("AGENT.md");
    if !default_agent.exists() {
        std::fs::write(&default_agent, "")?;
    }

    // USER.md — personal instructions, gitignored
    let user_md = gait_dir.join("USER.md");
    if !user_md.exists() {
        std::fs::write(&user_md, "")?;
    }

    // skills/ — installed skills (disk presence = available)
    std::fs::create_dir_all(gait_dir.join("skills"))?;

    // sessions/ — turn history
    std::fs::create_dir_all(gait_dir.join("sessions"))?;

    // gait.jsonc — default config (never clobber).
    // allow must match gait_core::config::defaults::default_allow().
    // sandbox.on matches default_sandbox().on; readable/writable use $CODEBASE
    // (equivalent at runtime since compute_scope auto-injects the codebase, but
    // explicit so users can see and extend the list).
    let config_path = gait_dir.join("gait.jsonc");
    if !config_path.exists() {
        let system_reads = gait_core::sandbox::platform_system_reads();
        let system_reads_comment = if system_reads.is_empty() {
            "  //   (none on this platform)".to_string()
        } else {
            format!("  //   {}", system_reads.join("  "))
        };
        let config = format!(
            r#"{{
  // gait configuration
  "model": "minimax/MiniMax-M3",
  // Allow list: tools the agent may call without a human approval prompt.
  "allow": ["read", "glob", "grep", "ls", "clarify", "inform"],
  // Sandbox: kernel write-enforcement (Seatbelt on macOS, Landlock on Linux).
  //
  // readable / writable accept paths and these special tokens:
  //   $CODEBASE  expands to the --codebase path at runtime
  //   ~/...      expands to your home directory
  //
  // Platform paths always readable regardless of 'readable' (OS bootstrap requirement):
{system_reads_comment}
  //
  // The codebase root is always readable+writable for tools that need it,
  // regardless of what you list here.
  "sandbox": {{
    "on": true,
    "readable": ["$CODEBASE"],
    "writable": ["$CODEBASE"],
    "network": false
  }},
  // Tier 1 config-defined tools: shell commands the LLM can call by name
  "tools": []
}}
"#
        );
        std::fs::write(&config_path, config)?;
    }

    // .gitignore — merge required entries, no duplicates
    let gi_path = gait_dir.join(".gitignore");
    let mut gi = std::fs::read_to_string(&gi_path).unwrap_or_default();
    let required = ["sessions/", "USER.md", ".gait-ws.lock"];
    let mut changed = false;
    for entry in &required {
        if !gi.lines().any(|l| l.trim() == *entry) {
            if !gi.is_empty() && !gi.ends_with('\n') {
                gi.push('\n');
            }
            gi.push_str(entry);
            gi.push('\n');
            changed = true;
        }
    }
    if changed {
        std::fs::write(&gi_path, &gi)?;
    }

    Ok(())
}

pub fn cmd_init(args: InitArgs) {
    let gait_dir = match args.path {
        Some(p) => p,
        None => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    };

    if let Err(e) = std::fs::create_dir_all(&gait_dir) {
        eprintln!(
            "[gait] cannot create directory {}: {}",
            gait_dir.display(),
            e
        );
        std::process::exit(1);
    }

    if let Err(e) = gait_core::git::Repos::production()
        .traces
        .init_workspace(&gait_dir)
    {
        eprintln!("[gait] git init failed: {}", e);
        std::process::exit(1);
    }

    if let Err(e) = scaffold_gait_dir(&gait_dir) {
        eprintln!("[gait] scaffold failed: {}", e);
        std::process::exit(1);
    }

    // Stage and initial commit via git2
    let repo = match git2::Repository::open(&gait_dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[gait] cannot open repo: {e}");
            std::process::exit(1);
        }
    };
    let sig = repo
        .signature()
        .or_else(|_| git2::Signature::now("gait", "gait@localhost"))
        .unwrap();
    let mut index = repo.index().unwrap();

    // Stage exactly what scaffold created (relative paths from gait_dir root)
    let to_stage = ["gait.jsonc", ".gitignore", "agents/default/AGENT.md"];
    for f in &to_stage {
        if gait_dir.join(f).exists() {
            let _ = index.add_path(std::path::Path::new(f));
        }
    }
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();

    let parent_commits: Vec<git2::Commit> = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .into_iter()
        .collect();
    let parents: Vec<&git2::Commit> = parent_commits.iter().collect();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "gait: init gait_dir",
        &tree,
        &parents,
    )
    .unwrap();

    print_init_summary(&gait_dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_allow_matches_no_config_default() {
        let dir = tempfile::tempdir().unwrap();
        run_init(dir.path()).unwrap();
        let cfg = gait_core::config::load_config(dir.path());
        assert_eq!(
            cfg.allow,
            gait_core::config::defaults::default_allow(),
            "scaffold allow must match default_allow()"
        );
        assert_eq!(
            cfg.sandbox.on,
            gait_core::config::defaults::default_sandbox().on,
            "scaffold sandbox.on must match default_sandbox().on"
        );
    }

    #[test]
    fn scaffold_creates_empty_affordances_and_gitignores_user() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "sessions/\n").unwrap();
        scaffold_gait_dir(dir.path()).unwrap();
        assert!(dir
            .path()
            .join("agents")
            .join("default")
            .join("AGENT.md")
            .exists());
        assert!(dir.path().join("USER.md").exists());
        let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(
            gi.lines().any(|l| l.trim() == "USER.md"),
            "USER.md gitignored: {gi}"
        );
        // Idempotent + non-clobbering:
        std::fs::write(dir.path().join("USER.md"), "mine").unwrap();
        scaffold_gait_dir(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("USER.md")).unwrap(),
            "mine"
        );
        assert_eq!(
            gi.matches("USER.md").count(),
            1,
            "no duplicate gitignore entry"
        );
    }

    mod init_tests {
        use super::*;

        #[test]
        fn scaffold_creates_new_dir_layout() {
            let dir = tempfile::TempDir::new().unwrap();
            scaffold_gait_dir(dir.path()).unwrap();

            assert!(
                dir.path()
                    .join("agents")
                    .join("default")
                    .join("AGENT.md")
                    .exists(),
                "agents/default/AGENT.md must exist"
            );
            assert!(
                !dir.path().join("agents").join("default.md").exists(),
                "agents/default.md (flat, old) must not be created"
            );
            assert!(dir.path().join("skills").is_dir(), "skills/ dir must exist");
            assert!(dir.path().join("USER.md").exists());
            let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap_or_default();
            assert!(
                gi.lines().any(|l| l.trim() == "USER.md"),
                "USER.md in .gitignore"
            );
            assert!(
                gi.lines().any(|l| l.trim() == "sessions/"),
                "sessions/ in .gitignore"
            );
        }

        #[test]
        fn scaffold_is_idempotent_and_does_not_clobber() {
            let dir = tempfile::TempDir::new().unwrap();
            std::fs::create_dir_all(dir.path().join("agents").join("default")).unwrap();
            std::fs::write(
                dir.path().join("agents").join("default").join("AGENT.md"),
                "my instructions",
            )
            .unwrap();
            std::fs::write(dir.path().join("USER.md"), "personal").unwrap();

            scaffold_gait_dir(dir.path()).unwrap();

            assert_eq!(
                std::fs::read_to_string(dir.path().join("agents/default/AGENT.md")).unwrap(),
                "my instructions",
                "must not clobber existing AGENT.md"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("USER.md")).unwrap(),
                "personal",
                "must not clobber existing USER.md"
            );
        }

        #[test]
        fn scaffold_gitignore_no_duplicate_entries() {
            let dir = tempfile::TempDir::new().unwrap();
            scaffold_gait_dir(dir.path()).unwrap();
            scaffold_gait_dir(dir.path()).unwrap();
            let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
            assert_eq!(gi.matches("USER.md").count(), 1, "USER.md appears once");
            assert_eq!(gi.matches("sessions/").count(), 1, "sessions/ appears once");
        }
    }
}
