//! The context every op receives, and the process-wide flags it is built from.
use futures_util::future::BoxFuture;
use op_registry::{CliContext, OpGlobals, Ref, Resolve, Resolved, Scope};
use std::path::{Path, PathBuf};

/// Process settings: the flags every command accepts, before or after the verb.
#[derive(OpGlobals, Debug, Clone, Default)]
pub struct Globals {
    /// The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/`
    /// directory or `.gaitref` file found walking up from the current
    /// directory are used.
    #[op(env = "GAIT_DIR")]
    pub gait_dir: Option<PathBuf>,
    /// Agent identity directory. Sets GAIT_AGENT_DIR for this process and every
    /// child it spawns.
    pub agent_dir: Option<PathBuf>,
}

/// What an op runs against. Built from the globals with no I/O: the gait
/// directory is resolved only when an op asks for it, so `--help` and `local:`
/// ops never need one.
#[derive(Debug, Clone)]
pub struct GaitContext {
    gait_dir_flag: Option<PathBuf>,
}

impl GaitContext {
    /// The gait directory: the flag (which also carries `$GAIT_DIR`), else the
    /// `.gaitref` / `.gait/` walk-up from the current directory.
    pub fn gait_dir(&self) -> anyhow::Result<PathBuf> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.gait_dir_from(&cwd)
    }

    /// Like [`GaitContext::gait_dir`], walking up from `start` instead of the
    /// current directory.
    pub fn gait_dir_from(&self, start: &Path) -> anyhow::Result<PathBuf> {
        gait_state::gait_dir::resolve(self.gait_dir_flag.clone(), start)
    }
}

impl CliContext for GaitContext {
    type Globals = Globals;

    fn from_globals(globals: &Globals) -> anyhow::Result<Self> {
        Ok(GaitContext { gait_dir_flag: globals.gait_dir.clone() })
    }
}

impl Resolve for GaitContext {
    fn resolve<'a>(
        &'a self,
        kind: &'a str,
        r: &'a Ref,
        _scope: &'a Scope,
    ) -> BoxFuture<'a, anyhow::Result<Resolved>> {
        Box::pin(async move {
            let name = r.as_raw();
            match kind {
                // Sessions are addressed by id only; their ops are `id_only`
                // and the existence check belongs to the op.
                "session" => {}
                "inference" => {
                    gait_ops::inference::show(&self.gait_dir()?, name)?;
                }
                "mode" => {
                    gait_ops::mode::show(&self.gait_dir()?, name)?;
                }
                "daemon" => {
                    let gait_dir = self.gait_dir()?;
                    let running = gait_state::session::daemons::active(&gait_dir)?;
                    if !running.iter().any(|d| d.name == name) {
                        return Err(coded_error::into_anyhow(gait_ops::OpError::NotFound(
                            format!("no daemon named '{name}'"),
                        )));
                    }
                }
                other => anyhow::bail!("context has no resolver for kind `{other}`"),
            }
            Ok(Resolved::Id(name.to_owned()))
        })
    }
}
