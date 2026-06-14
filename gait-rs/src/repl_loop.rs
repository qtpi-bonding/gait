//! REPL adapters for the unified `run_loop` (Phase 4b): a `ReplSource` (line
//! reader + interaction/respawn), a `ReplSink` (view-layer bridge), and a
//! per-turn `ReplDriver`. `ReplSource` and `ReplSink` share one `ReplBuilder`
//! behind `Rc<RefCell<…>>`; the loop borrows them at disjoint times (drive, then
//! next), so the cell never double-borrows. The loop is awaited (not spawned),
//! so the non-`Send` `Rc` is fine.
use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::embed::{user_turn_body, LoopConfig, TurnDriver};
use crate::port::input::{TurnSource, TurnStart};
use crate::port::output::{Diag, TurnSink};
use crate::proto::gait::log::v1::StreamItem;
use crate::proto::{events, wire};
use crate::repl_helper::ShellNavState;
use crate::repl_input::{bang_command_str, expand_input, is_bang_command, parse_skill_toggle,
    run_and_capture_pty_raw, CaptureResult, ExpandCtx};
use crate::theme::{Theme, RESET};
use crate::turn::TurnResult;
use crate::ui::{ReplBuilder, ViewSignal};

/// A session-control signal the source raises to end the current `run_loop`.
pub enum Control { NewSession(String), Quit }

/// Single-threaded REPL state shared by `ReplSink` and `ReplSource`.
pub struct ReplShared {
    pub builder: ReplBuilder,
    /// Blocks captured from the just-finished turn's `TurnDone` signal (preferred
    /// over `take_turn_blocks()` — they may differ when `turn_complete` carries
    /// its own blocks).
    pub turn_done_blocks: Option<Vec<String>>,
    pub control: Option<Control>,
}
pub type Shared = Rc<RefCell<ReplShared>>;

/// View-layer sink: forwards stream items to the builder, captures `TurnDone`.
pub struct ReplSink { pub shared: Shared }
impl TurnSink for ReplSink {
    fn item(&mut self, it: &StreamItem) {
        let mut s = self.shared.borrow_mut();
        if let ViewSignal::TurnDone(b) = s.builder.handle_stream_item(it.clone()) {
            s.turn_done_blocks = Some(b);
        }
    }
    fn result(&mut self, _r: &TurnResult) {}      // REPL never prints the proto RunResult
    fn session_id(&mut self, _id: &str) {}
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        if level == Diag::Error { eprintln!("[gait] {msg}"); }
    }
    fn begin_turn(&mut self) { self.shared.borrow_mut().builder.begin_turn(); }
    fn end_turn(&mut self) { self.shared.borrow_mut().builder.flush(); }
}

/// Per-turn driver: rebuilds the Profile each turn (matches the old REPL), no
/// after-turn coordination, and keeps the REPL alive on a turn error.
pub struct ReplDriver {
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub agent_path: Option<PathBuf>,
    pub session_id: String,
    pub config: crate::config::GaitConfig,
    pub repos: crate::git::Repos,
}
#[async_trait::async_trait(?Send)]
impl TurnDriver for ReplDriver {
    async fn caps(&mut self) -> anyhow::Result<crate::profile::ResolvedProfile> {
        let profile = crate::profile::Profile::from_config_and_agent(
            &self.config, &self.gait_dir, Some(self.codebase.as_path()), self.agent_path.as_deref(), None);
        let api_key = crate::auth::resolve_api_key(
            profile.model.split_once('/').map(|(p, _)| p).unwrap_or(&profile.model));
        profile.resolve(self.codebase.clone(), self.session_id.clone(), self.gait_dir.clone(), self.repos.clone(), api_key).await
    }
    fn turn_config(&self) -> LoopConfig {
        LoopConfig { auto: false, max_iter: self.config.max_iter, preview_policy: self.config.preview }
    }
    fn on_turn_error(&mut self, _sink: &mut dyn TurnSink, e: anyhow::Error) -> std::ops::ControlFlow<()> {
        eprintln!("[gait] turn error: {e}");
        std::ops::ControlFlow::Continue(())   // return to the prompt; don't kill the REPL
    }
}

/// Line reader + interaction/respawn. Yields `Input(user_turn)` per task line,
/// `Resume` after appending an interaction reply, `None` on `/n`/`/q`/EOF (with a
/// `Control` set so `run_chat` can start a new session or exit).
pub struct ReplSource {
    pub shared: Shared,
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub session_id: String,
    pub agent_path: Option<PathBuf>,
    pub repos: crate::git::Repos,
    pub theme: Theme,
    pub nav: Arc<Mutex<ShellNavState>>,
    pub shell_history_path: PathBuf,
    pub gait_bin: PathBuf,
    pub last_blocks: Vec<String>,
    pub ran_turn: bool,
}

impl ReplSource {
    fn log_path(&self) -> PathBuf {
        self.gait_dir.join("sessions").join(&self.session_id).join("log.jsonl")
    }
}

#[async_trait::async_trait(?Send)]
impl TurnSource for ReplSource {
    async fn next(&mut self) -> Option<TurnStart> {
        // ---- post-turn: capture blocks; if the turn left a pending interaction,
        // run it, append the reply, and respawn (fold it) ----
        if self.ran_turn {
            let interaction = {
                let mut s = self.shared.borrow_mut();
                let signal_blocks = s.turn_done_blocks.take();
                self.last_blocks = signal_blocks.unwrap_or_else(|| s.builder.take_turn_blocks());
                if s.builder.has_interaction() {
                    Some(s.builder.run_interaction(&self.gait_dir, &self.session_id, self.agent_path.as_deref()))
                } else {
                    None
                }
            };
            match interaction {
                Some(Ok(outcome)) => {
                    if wire::append(&self.log_path(), outcome.event_body).is_ok() && outcome.respawn {
                        return Some(TurnStart::Resume);   // fold the appended reply (no re-append)
                    }
                    println!();
                }
                Some(Err(e)) => {
                    eprintln!("[gait] {e}");
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
                None => { println!(); }   // turn done, no interaction
            }
        }
        self.ran_turn = false;

        // ---- read + dispatch lines until a task line (or a control signal) ----
        loop {
            self.nav.lock().unwrap().index = 0;
            let prompt = format!("{}>{} {}", self.theme.accent.fg(), RESET, self.theme.user_msg.fg());
            // Bind readline to a local so the RefCell borrow is released before the
            // match arms (which borrow `shared` again).
            let read = self.shared.borrow_mut().builder.readline(&prompt);
            let line = match read {
                Ok(l) => { eprint!("{}", RESET); l.trim().to_string() }
                Err(rustyline::error::ReadlineError::Eof)
                | Err(rustyline::error::ReadlineError::Interrupted) => {
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
                Err(_) => {
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
            };
            if line.is_empty() { continue; }

            if is_bang_command(&line) {
                let cmd = bang_command_str(&line);
                let (output, code) = match run_and_capture_pty_raw(cmd, &self.codebase) {
                    CaptureResult::Output(out, code) => (out, code),
                    CaptureResult::Interactive => (String::new(), Some(0)),
                };
                if code != Some(0) { eprintln!("[!] exit {}", code.unwrap_or(1)); }
                let _ = crate::shell_history::append(
                    &self.shell_history_path, cmd, &output, crate::session::transcript::now_ms());
                continue;
            }

            // /skillname [off] — emit a Toggle event; do NOT forward to the model.
            if let Some((skill_name, enabled)) = parse_skill_toggle(&line) {
                let available = crate::skills::scan_skills(&self.gait_dir);
                if !available.iter().any(|s| s.name == skill_name) {
                    eprintln!("[gait] no such skill {:?} — is skills/{}/SKILL.md present?", skill_name, skill_name);
                } else {
                    let _ = wire::append(&self.log_path(), events::toggle("skill", &skill_name, enabled));
                    eprintln!("skill {} {}", skill_name, if enabled { "enabled" } else { "disabled" });
                }
                continue;
            }

            match line.as_str() {
                "/n" | "/new" => {
                    self.shared.borrow_mut().control = Some(Control::NewSession(crate::chat::new_session_id()));
                    return None;
                }
                "/q" | "/exit" | "exit" | "quit" => {
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
                _ => {}
            }

            // /u [N], /uc [N], /ut [N]: undo both axes / code-only / transcript-only.
            let line: String = if line.starts_with("/uc") || line.starts_with("/ut") || line.starts_with("/u") {
                let (flags, label, arg_start) = if line.starts_with("/uc") {
                    (["--code"].as_slice(), "code", 3usize)
                } else if line.starts_with("/ut") {
                    (["--transcript"].as_slice(), "transcript", 3usize)
                } else {
                    (["--code", "--transcript"].as_slice(), "both", 2usize)
                };
                let arg = line[arg_start..].trim().to_owned();
                if arg.is_empty() {
                    let _ = Command::new(&self.gait_bin)
                        .args(["undo", &self.session_id, "--list",
                               "--gait-dir", self.gait_dir.to_str().unwrap_or(".")])
                        .status();
                    continue;
                } else if let Ok(n) = arg.parse::<u32>() {
                    let mut cmd_args = vec!["undo", &self.session_id, "--turn", &arg];
                    cmd_args.extend_from_slice(flags);
                    cmd_args.extend_from_slice(&["--gait-dir", self.gait_dir.to_str().unwrap_or(".")]);
                    let ok = Command::new(&self.gait_bin).args(&cmd_args).status()
                        .map(|s| s.success()).unwrap_or(false);
                    if !ok { eprintln!("[undo] failed — is there a turn {n} for this session?"); continue; }
                    format!("[undo] restored {label} to turn {n}")
                } else {
                    eprintln!("{}", crate::chat::hints::UNDO_USAGE);
                    continue;
                }
            } else {
                line
            };

            self.shared.borrow_mut().builder.add_history_entry(&line);
            // Unified expansion (identical to the CLI path): $(cmd)/$(N), @path, >(N).
            let expanded = expand_input(&line, &ExpandCtx {
                codebase: &self.codebase,
                history: &self.shell_history_path,
                last_blocks: &self.last_blocks,
            });
            let body = user_turn_body(&self.repos, &self.codebase, expanded);
            self.ran_turn = true;
            return Some(TurnStart::Input(body));
        }
    }
}