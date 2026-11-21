//! Workspace ops: `init` scaffolds a gait directory and `link` points the
//! current directory at one. Both are local: they take the directory from
//! `--gait-dir` (the process-wide flag) and never resolve the context's own
//! gait directory, so neither needs one to exist.
use super::{infra, GaitContext};
use op_registry::{CallOptions, OpParams};
use std::path::PathBuf;

output! {
    /// One file `init` scaffolds in a new gait directory.
    pub struct ScaffoldFile {
        pub path: String,
        pub purpose: String,
        /// Whether git tracks it.
        pub git: String,
    }
}

output! {
    /// The gait directory `init` created or found.
    pub struct InitResult {
        /// The gait directory, as an absolute path.
        pub gait_dir: String,
        /// True when the directory already was a gait directory.
        pub pre_existing_workspace: bool,
        /// True when it lies inside a codebase's git repository.
        pub in_tree: bool,
        /// True when a line was added to that repository's `.gitignore`.
        pub gitignored: bool,
        pub gitignore_line: Option<String>,
        pub gitignore_path: Option<String>,
        /// The files a new gait directory holds.
        pub files: Vec<ScaffoldFile>,
    }
}

output! {
    /// The pointer `link` wrote.
    pub struct LinkResult {
        /// The `.gaitref` file, in the current directory.
        pub gaitref: String,
        /// The gait directory it names.
        pub target: String,
    }
}

#[derive(Debug, Clone, OpParams)]
pub struct InitParams {
    /// Override a safety check: `init` may create the gait directory inside a codebase's git repository (a turn there then needs `--allow-intree`), `discard --all` may discard more than one session, `manual install` replaces an existing manual.
    pub force: bool,
}

#[derive(Debug, Clone, OpParams)]
pub struct LinkParams {}

fn scaffold_files() -> Vec<ScaffoldFile> {
    let file = |path: &str, purpose: &str, git: &str| ScaffoldFile {
        path: path.into(),
        purpose: purpose.into(),
        git: git.into(),
    };
    vec![
        file("gait.jsonc", "model + tool-permission config", "committed"),
        file(
            "agents/default/AGENT.md",
            "the agent's role and instructions",
            "committed",
        ),
        file(
            "USER.md",
            "your personal instructions (not shared)",
            "ignored",
        ),
        file(
            "skills/",
            "installed skills (skills/<n>/SKILL.md)",
            "committed (empty dir; tracked once you add skills)",
        ),
        file("sessions/", "turn history", "ignored"),
    ]
}

/// The directory `--gait-dir` names, made absolute. Without it the command is
/// a usage error: neither `init` nor `link` guesses a directory.
fn target(ctx: &GaitContext) -> anyhow::Result<PathBuf> {
    let Some(dir) = ctx.gait_dir_flag() else {
        return Err(coded_error::into_anyhow(
            op_registry::UsageError::MissingField {
                name: "gait-dir".to_owned(),
            },
        ));
    };
    Ok(if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(dir)
    })
}

/// Create a gait directory at `--gait-dir`: config, a default agent, skills and sessions, as a git repository. Running it on an existing gait directory changes nothing.
pub async fn init(
    ctx: &GaitContext,
    params: InitParams,
    opts: CallOptions,
) -> anyhow::Result<InitResult> {
    let outcome = gait_ops::workspace::init(gait_ops::workspace::InitParams {
        path: target(ctx)?,
        force: params.force,
    })
    .map_err(infra)?;
    if outcome.in_tree {
        opts.notices.notice(
            "this gait directory is inside your code tree; running it will require \
             --allow-intree, and an agent could then write to its own config.",
        );
    }
    Ok(InitResult {
        gait_dir: outcome.gait_dir.display().to_string(),
        pre_existing_workspace: outcome.pre_existing_workspace,
        in_tree: outcome.in_tree,
        gitignored: outcome.gitignored,
        gitignore_line: outcome.gitignore_line,
        gitignore_path: outcome.gitignore_path.map(|p| p.display().to_string()),
        files: scaffold_files(),
    })
}

/// Write a `.gaitref` file in the current directory naming the gait directory at `--gait-dir`, so later commands here find it without the flag.
pub async fn link(
    ctx: &GaitContext,
    _params: LinkParams,
    _opts: CallOptions,
) -> anyhow::Result<LinkResult> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let out = gait_ops::workspace::link(&cwd, &target(ctx)?).map_err(infra)?;
    Ok(LinkResult {
        gaitref: out.gaitref.display().to_string(),
        target: out.target.display().to_string(),
    })
}
