//! `gait init` — scaffold a gait-dir and make an initial commit.

use clap::Parser;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct InitArgs {
    /// Create the workspace even if it would live inside a codebase git repo.
    /// (In-tree is discouraged: running such a workspace also needs
    /// `--allow-intree`, since the agent could otherwise write to its own config.
    /// Prefer out-of-tree: `gait init ~/gait-home && gait link ~/gait-home`.)
    #[arg(long)]
    pub force: bool,
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

    // .active/ — live (not-yet-persisted) session worktree mounts. `sessions/`
    // itself is NOT pre-created here — it comes into existence via git
    // checkout the first time a session is persisted into main.
    std::fs::create_dir_all(gait_dir.join(".active"))?;

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
    let required = [".active/", "USER.md", ".gait-ws.lock"];
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

/// Nearest ancestor of `start` (inclusive) that contains a `.git`.
fn enclosing_git_repo(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// The `.gitignore` line (`<relpath>/`) for `gait_dir` within `repo_root`, or
/// `None` if `gait_dir` is not under `repo_root`.
fn gitignore_line_for(repo_root: &Path, gait_dir: &Path) -> Option<String> {
    let rel = gait_dir.strip_prefix(repo_root).ok()?;
    Some(format!("{}/", rel.display()))
}

/// True if `path` looks like an initialized gait workspace. Keyed off `.active/`
/// — a directory `scaffold_gait_dir` always creates. (Do NOT use `.gait-ws.lock`:
/// it is only a `.gitignore` line, never a real file, so it would be absent on
/// every real workspace and make the clobber guard refuse idempotent re-init.)
fn is_gait_workspace(path: &Path) -> bool {
    path.join(".active").exists()
}

/// The init decision for `target`. `Ok(codebase_repo_if_any)` → proceed (the
/// repo, if `Some`, is only present under `--force` in-tree and is used for the
/// later auto-gitignore). `Err(enclosing_repo)` → refuse. Pure — no writes, no
/// exit, no message: `cmd_init` renders the full guidance from the returned repo,
/// so the actionable text (out-of-tree flow, --force, --allow-intree) is not lost.
fn init_decision(target: &Path, force: bool) -> Result<Option<PathBuf>, PathBuf> {
    let codebase_repo = if is_gait_workspace(target) {
        None
    } else {
        enclosing_git_repo(target)
    };
    match codebase_repo {
        Some(repo) if !force => Err(repo),
        other => Ok(other),
    }
}

pub fn cmd_init(args: InitArgs) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let raw = args.path.clone().unwrap_or_else(|| cwd.clone());
    let gait_dir = if raw.is_absolute() {
        raw
    } else {
        cwd.join(raw)
    };

    // Decide (pre-creation) whether to proceed and, if so, capture the enclosing
    // codebase repo for later auto-gitignore. An existing gait workspace re-init
    // is exempt from the steer-away guard (idempotency preserved).
    let codebase_repo: Option<PathBuf> = match init_decision(&gait_dir, args.force) {
        Ok(repo) => repo,
        Err(repo) => {
            eprintln!(
                "[gait] {} would live inside git repo {} — an agent could then write \
                 to its own config there.\n       Prefer out-of-tree: `gait init \
                 ~/gait-home && gait link ~/gait-home`.\n       Or pass --force to \
                 create it here anyway (running it will also require --allow-intree).",
                gait_dir.display(),
                repo.display()
            );
            std::process::exit(1);
        }
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

    // Auto-gitignore: only reachable when --force created a workspace inside a
    // codebase repo. Merge the `<relpath>/` line into the codebase .gitignore
    // (create if absent, no duplicates), and print the in-tree scope warning.
    if let Some(repo_root) = &codebase_repo {
        if let Some(line) = gitignore_line_for(repo_root, &gait_dir) {
            let gi_path = repo_root.join(".gitignore");
            let existing = std::fs::read_to_string(&gi_path).unwrap_or_default();
            if !existing.lines().any(|l| l.trim() == line.trim()) {
                let mut out = existing;
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&line);
                out.push('\n');
                if std::fs::write(&gi_path, out).is_ok() {
                    println!("[gait] added {} to {}", line.trim(), gi_path.display());
                }
            }
        }
        eprintln!(
            "[gait] warning: this workspace is inside your code tree; running it will \
             require --allow-intree, and an agent could then write to its own config."
        );
    }

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
    fn init_creates_active_dir_not_sessions_and_gitignore_omits_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path();
        scaffold_gait_dir(gait_dir).unwrap();
        assert!(gait_dir.join(".active").exists(), ".active/ must exist");
        assert!(
            !gait_dir.join("sessions").exists(),
            "sessions/ must not be pre-created — it comes into existence via git \
             checkout once a session is persisted"
        );
        let gi = std::fs::read_to_string(gait_dir.join(".gitignore")).unwrap();
        assert!(
            !gi.lines().any(|l| l.trim() == "sessions/"),
            "root .gitignore must not list sessions/ (it's tracked, persisted content now): {gi}"
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
                gi.lines().any(|l| l.trim() == ".active/"),
                ".active/ in .gitignore"
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
            assert_eq!(gi.matches(".active/").count(), 1, ".active/ appears once");
        }
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn gitignore_line_is_relative_with_trailing_slash() {
        let repo = PathBuf::from("/home/me/proj");
        let gait_dir = PathBuf::from("/home/me/proj/.gait");
        assert_eq!(gitignore_line_for(&repo, &gait_dir).unwrap(), ".gait/");
    }

    #[test]
    fn gitignore_line_none_when_outside_repo() {
        let repo = PathBuf::from("/home/me/proj");
        let gait_dir = PathBuf::from("/home/me/gait-home");
        assert_eq!(gitignore_line_for(&repo, &gait_dir), None);
    }

    #[test]
    fn enclosing_repo_found_from_subdir() {
        let t = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(t.path().join(".git")).unwrap();
        let sub = t.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(enclosing_git_repo(&sub), Some(t.path().to_path_buf()));
    }

    #[test]
    fn is_gait_workspace_detects_active_dir_marker() {
        let t = tempfile::TempDir::new().unwrap();
        assert!(!is_gait_workspace(t.path()));
        std::fs::create_dir(t.path().join(".active")).unwrap();
        assert!(is_gait_workspace(t.path()));
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;

    #[test]
    fn existing_workspace_reinit_is_allowed_even_inside_a_repo() {
        let t = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(t.path().join(".git")).unwrap(); // codebase repo
        let ws = t.path().join(".gait");
        std::fs::create_dir_all(ws.join(".active")).unwrap(); // already a gait ws
        assert!(
            init_decision(&ws, false).is_ok(),
            "re-init must stay idempotent"
        );
    }

    #[test]
    fn fresh_target_inside_repo_is_refused_without_force() {
        let t = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(t.path().join(".git")).unwrap();
        assert!(init_decision(&t.path().join(".gait"), false).is_err());
        assert!(init_decision(&t.path().join(".gait"), true).is_ok()); // --force proceeds
    }

    #[test]
    fn out_of_tree_target_is_allowed() {
        let t = tempfile::TempDir::new().unwrap(); // no .git above
        assert_eq!(init_decision(&t.path().join("home"), false).unwrap(), None);
    }
}
