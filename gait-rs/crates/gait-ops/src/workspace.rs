//! Workspace verbs: `link` (write a .gaitref pointer), `init` (scaffold a gait-dir).
use crate::OpError;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct LinkOutcome {
    pub gaitref: PathBuf,
    pub target: PathBuf,
}

pub fn link(cwd: &Path, target: &Path) -> Result<LinkOutcome, OpError> {
    if !target.join(".git").exists() {
        return Err(OpError::NotFound(format!(
            "{} is not a gait workspace (no .git) — run `gait init {}` first",
            target.display(),
            target.display()
        )));
    }
    let gaitref = cwd.join(".gaitref");
    std::fs::write(&gaitref, format!("gaitdir: {}\n", target.display()))
        .map_err(anyhow::Error::from)?;
    Ok(LinkOutcome {
        gaitref,
        target: target.to_path_buf(),
    })
}

// -----------------------------------------------------------------------------
// init
// -----------------------------------------------------------------------------

#[derive(Debug)]
pub struct InitParams {
    /// Target gait-dir to create.
    pub path: PathBuf,
    /// Create the workspace even if it would live inside a codebase git repo.
    pub force: bool,
}

#[derive(Debug)]
pub struct InitOutcome {
    /// The path of the created (or existing) gait-dir.
    pub gait_dir: PathBuf,
    /// True if a line was added to a codebase repo's `.gitignore`.
    pub gitignored: bool,
    /// The line (relative path with trailing slash) added to the gitignore.
    pub gitignore_line: Option<String>,
    /// The path of the codebase `.gitignore` we wrote to.
    pub gitignore_path: Option<PathBuf>,
    /// True if the target already looked like a gait workspace (idempotent re-init).
    pub pre_existing_workspace: bool,
    /// True if the workspace is inside a codebase git repo (auto-gitignored).
    pub in_tree: bool,
}

/// Thin wrapper around [`scaffold_gait_dir`] for use in tests and programmatic
/// callers that want to initialize a gait-dir without the git init / staging /
/// commit steps.
#[doc(hidden)]
pub fn run_init(gait_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(gait_dir)?;
    scaffold_gait_dir(gait_dir)
}

/// Create the empty, editable layer affordances in the gait-dir (idempotent,
/// never clobbers) and ensure USER.md and sessions/ are gitignored.
pub(crate) fn scaffold_gait_dir(gait_dir: &Path) -> std::io::Result<()> {
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
    // allow must match gait_state::config::defaults::default_allow().
    // sandbox.on matches default_sandbox().on; readable/writable use $CODEBASE
    // (equivalent at runtime since compute_scope auto-injects the codebase, but
    // explicit so users can see and extend the list).
    let config_path = gait_dir.join("gait.jsonc");
    if !config_path.exists() {
        let system_reads = gait_state::sandbox::platform_system_reads();
        let system_reads_comment = if system_reads.is_empty() {
            "  //   (none on this platform)".to_string()
        } else {
            format!("  //   {}", system_reads.join("  "))
        };
        let deny_patterns_json = gait_state::config::defaults::default_deny_patterns()
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let config = format!(
            r#"{{
  // gait configuration
  // Pick a provider via the named-inference system: "inference" holds a
  // name that resolves through inferences/<name>/INFERENCE.yaml to the
  // actual provider/model-id string. The default below is bootstrap-scaffolded
  // by scaffold_gait_dir alongside this file. See `gait inference --help`.
  "inference": "minimax-m3",
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
    "network": false,
    // Deny: patterns denied even within the readable/writable scope above.
    // This list IS what's enforced — edit or remove entries to change it,
    // no hidden merge with a built-in list happens behind your back.
    // Enforced in gait's own code (an app-level path/glob check before the
    // read/write/apply_patch tools touch disk) — NOT by the OS sandbox, and
    // NOT for the shell tool (it can touch arbitrary paths gait can't
    // enumerate without parsing shell syntax).
    "deny": {{
      "read": [{deny_patterns_json}],
      "write": [{deny_patterns_json}]
    }}
  }},
  // Tier 1 config-defined tools: shell commands the LLM can call by name
  "tools": []
}}
"#
        );
        std::fs::write(&config_path, config)?;
    }

    // inferences/minimax-m3/ — default named inference. With
    // `"inference": "minimax-m3"` in gait.jsonc above, fresh installs work
    // out of the box: the name resolves through this file to
    // `minimax/MiniMax-M3`. Idempotent — never clobbers a hand-edited file.
    let inf_dir = gait_dir.join("inferences").join("minimax-m3");
    std::fs::create_dir_all(&inf_dir)?;
    let inf_file = inf_dir.join("INFERENCE.yaml");
    if !inf_file.exists() {
        std::fs::write(&inf_file, "model: minimax/MiniMax-M3\n")?;
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
/// exit, no message: the adapter renders the full guidance from the returned
/// repo, so the actionable text (out-of-tree flow, --force, --allow-intree) is
/// not lost.
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

fn is_global_gait_home(target: &Path, home: &gait_state::gait_home::GaitHome) -> bool {
    let absolute_target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(target)
    };
    absolute_target == home.as_path()
}

/// Initialize a gait-dir, committing the scaffold. The CLI adapter renders
/// success / error; this op carries no terminal I/O.
pub fn init(params: InitParams) -> Result<InitOutcome, OpError> {
    let gait_dir = params.path;

    if let Ok(home) = gait_state::gait_home::GaitHome::resolve() {
        if is_global_gait_home(&gait_dir, &home) {
            return Err(OpError::Conflict(format!(
                "{} is GAIT_HOME and cannot be initialized as a gait workspace",
                gait_dir.display()
            )));
        }
    }

    // Decide (pre-creation) whether to proceed and, if so, capture the enclosing
    // codebase repo for later auto-gitignore. An existing gait workspace re-init
    // is exempt from the steer-away guard (idempotency preserved).
    let codebase_repo: Option<PathBuf> = match init_decision(&gait_dir, params.force) {
        Ok(repo) => repo,
        Err(repo) => {
            return Err(OpError::Conflict(format!(
                "{} would live inside git repo {} — an agent could then write \
                 to its own config there.\n       Prefer out-of-tree: `gait init \
                 ~/gait-home && gait link ~/gait-home`.\n       Or pass --force to \
                 create it here anyway (running it will also require --allow-intree).",
                gait_dir.display(),
                repo.display()
            )));
        }
    };

    let pre_existing_workspace = is_gait_workspace(&gait_dir);

    std::fs::create_dir_all(&gait_dir).map_err(anyhow::Error::from)?;

    gait_state::git::Repos::production()
        .traces
        .init_workspace(&gait_dir)?;

    scaffold_gait_dir(&gait_dir).map_err(anyhow::Error::from)?;

    // Stage and initial commit via git2
    let repo = git2::Repository::open(&gait_dir).map_err(anyhow::Error::from)?;
    let sig = repo
        .signature()
        .or_else(|_| git2::Signature::now("gait", "gait@localhost"))
        .map_err(anyhow::Error::from)?;
    let mut index = repo.index().map_err(anyhow::Error::from)?;

    // Stage exactly what scaffold created (relative paths from gait_dir root)
    let to_stage = ["gait.jsonc", ".gitignore", "agents/default/AGENT.md"];
    for f in &to_stage {
        if gait_dir.join(f).exists() {
            let _ = index.add_path(std::path::Path::new(f));
        }
    }
    index.write().map_err(anyhow::Error::from)?;
    let tree_id = index.write_tree().map_err(anyhow::Error::from)?;
    let tree = repo.find_tree(tree_id).map_err(anyhow::Error::from)?;

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
    .map_err(anyhow::Error::from)?;

    // Auto-gitignore: only reachable when --force created a workspace inside a
    // codebase repo. Merge the `<relpath>/` line into the codebase .gitignore
    // (create if absent, no duplicates).
    let mut gitignored = false;
    let mut gitignore_line: Option<String> = None;
    let mut gitignore_path: Option<PathBuf> = None;
    let in_tree = codebase_repo.is_some();

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
                std::fs::write(&gi_path, out).map_err(anyhow::Error::from)?;
                gitignored = true;
                gitignore_line = Some(line.trim().to_string());
                gitignore_path = Some(gi_path);
            }
        }
    }

    Ok(InitOutcome {
        gait_dir,
        gitignored,
        gitignore_line,
        gitignore_path,
        pre_existing_workspace,
        in_tree,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_gait_home_is_not_a_workspace_target() {
        let home =
            gait_state::gait_home::GaitHome::from_path(PathBuf::from("/Users/test/.gait-home"))
                .unwrap();
        assert!(is_global_gait_home(
            Path::new("/Users/test/.gait-home"),
            &home
        ));
        assert!(!is_global_gait_home(
            Path::new("/Users/test/workspaces/example"),
            &home
        ));
    }

    #[test]
    #[serial_test::serial]
    fn link_writes_gaitref_pointing_at_target() {
        // `resolve(None, ...)` honors $GAIT_DIR before walk-up (by design), so a
        // dev shell with $GAIT_DIR exported would otherwise short-circuit this
        // test before it ever reaches the .gaitref walk-up being verified here.
        let saved = std::env::var("GAIT_DIR").ok();
        std::env::remove_var("GAIT_DIR");

        let t = tempfile::TempDir::new().unwrap();
        let target = t.path().join("home");
        std::fs::create_dir_all(target.join(".git")).unwrap();
        let cwd = t.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let out = link(&cwd, &target).unwrap();
        assert_eq!(out.gaitref, cwd.join(".gaitref"));
        let body = std::fs::read_to_string(cwd.join(".gaitref")).unwrap();
        assert_eq!(body, format!("gaitdir: {}\n", target.display()));
        // Round-trips through the core resolver.
        let result = gait_state::gait_dir::resolve(None, &cwd);
        if let Some(v) = saved {
            std::env::set_var("GAIT_DIR", v);
        }
        assert_eq!(result.unwrap(), target);
    }

    #[test]
    fn link_rejects_non_workspace_target() {
        let t = tempfile::TempDir::new().unwrap();
        let target = t.path().join("not-a-repo");
        std::fs::create_dir_all(&target).unwrap(); // no .git
        let err = link(t.path(), &target).unwrap_err();
        assert!(matches!(err, OpError::NotFound(_)));
    }

    // -- init helpers --------------------------------------------------------

    #[test]
    fn scaffold_allow_matches_no_config_default() {
        let dir = tempfile::tempdir().unwrap();
        run_init(dir.path()).unwrap();
        let cfg = gait_state::config::load_config(dir.path());
        assert_eq!(
            cfg.allow,
            gait_state::config::defaults::default_allow(),
            "scaffold allow must match default_allow()"
        );
        assert_eq!(
            cfg.sandbox.on,
            gait_state::config::defaults::default_sandbox().on,
            "scaffold sandbox.on must match default_sandbox().on"
        );
    }

    #[test]
    fn scaffolded_gait_jsonc_deny_matches_default_deny_patterns() {
        let dir = tempfile::TempDir::new().unwrap();
        run_init(dir.path()).unwrap();
        let config_text = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        let parsed = gait_state::config::load_config(dir.path()); // infallible -> GaitConfig
        let expected = gait_state::config::defaults::default_deny_patterns();
        assert_eq!(parsed.sandbox.deny.read, expected);
        assert_eq!(parsed.sandbox.deny.write, expected);
        // Also assert the scaffold TEXT itself names the patterns, not just that
        // parsing resolves to them via the serde default — this is what makes them
        // visible/editable, the actual point of writing them into the scaffold.
        assert!(
            config_text.contains("\"deny\""),
            "scaffold must literally write sandbox.deny"
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
}
