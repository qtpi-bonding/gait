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

use crate::repl::ShellNavState;
use crate::repl::{bang_command_str, is_bang_command};
use crate::theme::{Theme, RESET};
use crate::ui::{ReplBuilder, ViewSignal};
use gait_core::embed::{user_turn_events, LoopConfig, TurnDriver};
use gait_core::expand::{expand_input, ExpandCtx};
use gait_core::port::input::{TurnSource, TurnStart};
use gait_core::port::output::{Diag, TurnSink};
use gait_core::proto::gait::log::v1::StreamItem;
use gait_core::proto::{events, wire};
use gait_core::pty::{run_and_capture_pty_raw, CaptureResult};
use gait_core::turn::TurnResult;

/// RAII guard: disables crossterm raw mode on drop.
#[allow(dead_code)]
struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
/// A session-control signal the source raises to end the current `run_loop`.
pub enum Control {
    NewSession(String),
    Quit,
}

/// Single-threaded REPL state shared by `ReplSink` and `ReplSource`.
pub struct ReplShared {
    pub builder: ReplBuilder,
    /// Blocks captured from the just-finished turn's `TurnDone` signal (preferred
    /// over `take_turn_blocks()` — they may differ when `turn_complete` carries
    /// its own blocks).
    pub turn_done_blocks: Option<Vec<String>>,
    pub control: Option<Control>,
    // Watcher lifecycle — written by ReplDriver::turn_config, torn down by ReplSink::end_turn
    pub(crate) watcher_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(crate) watcher_handle: Option<tokio::task::JoinHandle<()>>,
    /// Per-session sandbox engagement, toggled by `/sandbox on|off`. Initialized
    /// from `config.sandbox.on`; takes effect on the next turn.
    pub sandbox_on: bool,
}
pub type Shared = Rc<RefCell<ReplShared>>;

/// View-layer sink: forwards stream items to the builder, captures `TurnDone`.
pub struct ReplSink {
    pub shared: Shared,
}
impl TurnSink for ReplSink {
    fn item(&mut self, it: &StreamItem) {
        let mut s = self.shared.borrow_mut();
        if let ViewSignal::TurnDone(b) = s.builder.handle_stream_item(it.clone()) {
            s.turn_done_blocks = Some(b);
        }
    }
    fn result(&mut self, r: &TurnResult) {
        match r {
            TurnResult::Cancelled => {
                eprint!("\x1b[2m⊘ interrupted\x1b[0m\r\n");
            }
            TurnResult::Unlock { items } => {
                eprint!(
                    "\r\n\x1b[33msandbox: {} item(s) need unlock\x1b[0m\r\n",
                    items.len()
                );
                for item in items {
                    eprint!(
                        "  \x1b[2m[{dim}] {resource}\x1b[0m\r\n",
                        dim = item.dimension,
                        resource = item.resource
                    );
                }
                eprint!("  \x1b[2mrun: /unlock [once|session|global|agent]\x1b[0m\r\n");
            }
            _ => {}
        }
        // structured_done rendered via ReplItem::StructuredResult (existing behaviour)
    }
    fn session_id(&mut self, _id: &str) {}
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        if level == Diag::Error {
            eprint!("[gait] {msg}\r\n");
        }
    }
    fn begin_turn(&mut self) {
        self.shared.borrow_mut().builder.begin_turn();
    }
    fn end_turn(&mut self) {
        let handle = {
            let mut s = self.shared.borrow_mut();
            s.watcher_done
                .store(true, std::sync::atomic::Ordering::SeqCst);
            s.watcher_handle.take()
        };
        // block_in_place joins the spawn_blocking watcher synchronously.
        // Watcher exits within 50ms (its poll interval). Requires multi-thread
        // runtime — #[tokio::main] default; do not use current_thread flavor.
        if let Some(h) = handle {
            tokio::task::block_in_place(|| {
                let _ = tokio::runtime::Handle::current().block_on(h);
            });
        }
        // Stop the spinner timer and flush any pending render state (was the
        // original end_turn body before watcher teardown moved here).
        self.shared.borrow_mut().builder.flush();
    }
}

/// Per-turn driver: rebuilds the Profile each turn (matches the old REPL), no
/// after-turn coordination, and keeps the REPL alive on a turn error.
/// Retained for ESC-key watcher infrastructure; not constructed in spine mode.
#[allow(dead_code)]
pub struct ReplDriver {
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub agent_path: Option<PathBuf>,
    pub session_id: String,
    pub config: gait_core::config::GaitConfig,
    pub repos: gait_core::git::Repos,
    pub shared: crate::repl::Shared,
}
#[async_trait::async_trait(?Send)]
impl TurnDriver for ReplDriver {
    async fn resolve_profile(&mut self) -> anyhow::Result<gait_core::profile::ResolvedProfile> {
        let runner = std::sync::Arc::new(gait_workflow::GaitWorkflowRunner::new(
            self.gait_dir.clone(),
            self.codebase.clone(),
        ));
        let provider_name = self
            .config
            .model
            .split_once('/')
            .map(|(p, _)| p)
            .unwrap_or(&self.config.model)
            .to_string();
        let api_key = gait_core::auth::resolve_api_key(&provider_name);
        gait_core::embed::resolve_profile(
            &self.config,
            &self.gait_dir,
            &self.codebase,
            &self.session_id,
            self.agent_path.as_deref(),
            None,
            &self.repos,
            Some(runner),
            api_key,
        )
        .await
    }
    fn turn_config(&mut self) -> LoopConfig {
        use tokio_util::sync::CancellationToken;

        // Fresh cancellation token + done flag for this turn.
        let token = CancellationToken::new();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_for_watcher = done.clone();
        let token_clone = token.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let _ = crossterm::terminal::enable_raw_mode();
            let _guard = RawModeGuard;
            loop {
                if done_for_watcher.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                if crossterm::event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
                    if let Ok(crossterm::event::Event::Key(e)) = crossterm::event::read() {
                        if e.code == crossterm::event::KeyCode::Esc {
                            token_clone.cancel();
                        }
                    }
                }
            }
        });
        {
            let mut s = self.shared.borrow_mut();
            s.watcher_done = done;
            s.watcher_handle = Some(handle);
        }

        LoopConfig {
            auto: false,
            max_iter: self.config.max_iter,
            preview_policy: self.config.preview,
            cancel: Some(token),
            sandbox_on: self.shared.borrow().sandbox_on,
            sandbox_cfg: self.config.sandbox.clone(),
            relay: None,
        }
    }

    async fn after_turn(
        &mut self,
        _sink: &mut dyn gait_core::port::output::TurnSink,
        _result: &gait_core::turn::TurnResult,
    ) {
        // Watcher is torn down in ReplSink::end_turn() before this runs.
    }

    fn on_turn_error(
        &mut self,
        _sink: &mut dyn TurnSink,
        e: anyhow::Error,
    ) -> std::ops::ControlFlow<()> {
        // Watcher is torn down in ReplSink::end_turn() before this runs.
        eprint!("[gait] turn error: {e}\r\n");
        std::ops::ControlFlow::Continue(())
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
    pub repos: gait_core::git::Repos,
    pub theme: Theme,
    pub nav: Arc<Mutex<ShellNavState>>,
    pub shell_history_path: PathBuf,
    pub gait_bin: PathBuf,
    pub last_blocks: Vec<String>,
    pub ran_turn: bool,
}

impl ReplSource {
    fn log_path(&self) -> PathBuf {
        self.gait_dir
            .join("sessions")
            .join(&self.session_id)
            .join("log.jsonl")
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
                    Some(s.builder.run_interaction(
                        &self.gait_dir,
                        &self.session_id,
                        self.agent_path.as_deref(),
                    ))
                } else {
                    None
                }
            };
            match interaction {
                Some(Ok(outcome)) => {
                    if wire::append(&self.log_path(), outcome.event_body).is_ok() && outcome.respawn
                    {
                        return Some(TurnStart::Resume); // fold the appended reply (no re-append)
                    }
                    eprint!("\r\n");
                }
                Some(Err(e)) => {
                    eprint!("[gait] {e}\r\n");
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
                None => {
                    eprint!("\r\n");
                } // turn done, no interaction
            }
        }
        self.ran_turn = false;

        // ---- read + dispatch lines until a task line (or a control signal) ----
        loop {
            self.nav.lock().unwrap().index = 0;
            let prompt = format!(
                "{}>{} {}",
                self.theme.accent.fg(),
                RESET,
                self.theme.user_msg.fg()
            );
            // Bind readline to a local so the RefCell borrow is released before the
            // match arms (which borrow `shared` again).
            let read = self.shared.borrow_mut().builder.readline(&prompt);
            let line = match read {
                Ok(l) => {
                    eprint!("{}", RESET);
                    l.trim().to_string()
                }
                Err(rustyline::error::ReadlineError::Eof) => {
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
                Err(rustyline::error::ReadlineError::Interrupted) => {
                    eprint!("{}", RESET);
                    continue;
                }
                Err(_) => {
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
            };
            if line.is_empty() {
                continue;
            }

            if is_bang_command(&line) {
                let cmd = bang_command_str(&line);
                let (output, code) = match run_and_capture_pty_raw(cmd, &self.codebase) {
                    CaptureResult::Output(out, code) => (out, code),
                    CaptureResult::Interactive => (String::new(), Some(0)),
                };
                if code != Some(0) {
                    eprint!("[!] exit {}\r\n", code.unwrap_or(1));
                }
                let _ = gait_core::shell_history::append(
                    &self.shell_history_path,
                    cmd,
                    &output,
                    gait_core::session::log::now_ms(),
                );
                continue;
            }

            // /sandbox [off|on] — toggle per-session sandbox engagement. Takes effect on
            // the next turn (ephemeral: never mutates a live sealed process).
            // Must be checked before parse_skill_toggle so "sandbox" isn't treated as
            // a skill name.
            if line.trim() == "/sandbox" || line.trim() == "/sandbox on" {
                self.shared.borrow_mut().sandbox_on = true;
                eprint!("sandbox enabled\r\n");
                continue;
            }
            if line.trim() == "/sandbox off" {
                self.shared.borrow_mut().sandbox_on = false;
                eprint!("sandbox disabled\r\n");
                continue;
            }

            // /unlock [scope] — delegate to `gait unlock` for the current session.
            // Scope defaults to "once". Mirrors /u N → gait undo.
            if let Some(scope_str) = line.strip_prefix("/unlock") {
                let scope = scope_str.trim();
                let scope = if scope.is_empty() { "once" } else { scope };
                match scope {
                    "once" | "session" | "global" | "agent" => {
                        let ok = Command::new(&self.gait_bin)
                            .args([
                                "unlock",
                                &self.session_id,
                                "--scope",
                                scope,
                                "--gait-dir",
                                self.gait_dir.to_str().unwrap_or("."),
                            ])
                            .status()
                            .map(|s| s.success())
                            .unwrap_or(false);
                        if !ok {
                            eprint!(
                                "[gait] unlock failed — check: gait status {} --gait-dir {}\r\n",
                                self.session_id,
                                self.gait_dir.display()
                            );
                        }
                    }
                    _ => {
                        eprint!("[gait] usage: /unlock [once|session|global|agent]\r\n");
                    }
                }
                continue;
            }

            // /skillname [off] — emit a Toggle event; do NOT forward to the model.
            if let Some((skill_name, enabled)) = parse_skill_toggle(&line) {
                let available = gait_core::skills::scan_skills(&self.gait_dir);
                if !available.iter().any(|s| s.name == skill_name) {
                    eprint!(
                        "[gait] no such skill {:?} — is skills/{}/SKILL.md present?\r\n",
                        skill_name, skill_name
                    );
                } else {
                    let _ = wire::append(
                        &self.log_path(),
                        events::toggle("skill", &skill_name, enabled),
                    );
                    eprint!(
                        "skill {} {}\r\n",
                        skill_name,
                        if enabled { "enabled" } else { "disabled" }
                    );
                }
                continue;
            }

            // /workflow <name> [args...] — start a named workflow non-blocking, print run_id.
            if let Some(rest) = line.strip_prefix("/workflow") {
                let mut parts = rest.split_whitespace();
                match parts.next() {
                    None => eprint!("[gait] usage: /workflow <name> [args...]\r\n"),
                    Some(name) => {
                        let args: Vec<String> = parts.map(String::from).collect();
                        let runner = gait_workflow::GaitWorkflowRunner::new(
                            self.gait_dir.clone(),
                            self.codebase.clone(),
                        );
                        use gait_core::workflow_runner::WorkflowRunner as _;
                        let result = runner.run(name.to_string(), args).await;
                        println!("{result}");
                    }
                }
                continue;
            }

            match line.as_str() {
                "/n" | "/new" => {
                    self.shared.borrow_mut().control =
                        Some(Control::NewSession(gait_core::session::new_session_id()));
                    return None;
                }
                "/q" | "/exit" | "exit" | "quit" => {
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
                _ => {}
            }

            // /u [N], /uc [N], /ut [N]: undo both axes / code-only / transcript-only.
            let line: String = if line.starts_with("/uc")
                || line.starts_with("/ut")
                || line.starts_with("/u")
            {
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
                        .args([
                            "undo",
                            &self.session_id,
                            "--list",
                            "--gait-dir",
                            self.gait_dir.to_str().unwrap_or("."),
                        ])
                        .status();
                    continue;
                } else if let Ok(n) = arg.parse::<u32>() {
                    let mut cmd_args = vec!["undo", &self.session_id, "--turn", &arg];
                    cmd_args.extend_from_slice(flags);
                    cmd_args
                        .extend_from_slice(&["--gait-dir", self.gait_dir.to_str().unwrap_or(".")]);
                    let ok = Command::new(&self.gait_bin)
                        .args(&cmd_args)
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if !ok {
                        eprint!("[undo] failed — is there a turn {n} for this session?\r\n");
                        continue;
                    }
                    format!("[undo] restored {label} to turn {n}")
                } else {
                    eprint!(
                        "{}\r\n",
                        crate::chat::hints::UNDO_USAGE.replace('\n', "\r\n")
                    );
                    continue;
                }
            } else {
                line
            };

            self.shared.borrow_mut().builder.add_history_entry(&line);
            // Unified expansion (identical to the CLI path): $(cmd)/$(N), @path, >(N).
            let expanded = expand_input(
                &line,
                &ExpandCtx {
                    codebase: &self.codebase,
                    history: &self.shell_history_path,
                    last_blocks: &self.last_blocks,
                },
            );
            let bodies = user_turn_events(&self.repos, &self.codebase, expanded);
            self.ran_turn = true;
            return Some(TurnStart::Input(bodies));
        }
    }
}

/// Reserved REPL built-in prefixes that must NOT be treated as skill names.
const BUILTIN_PREFIXES: &[&str] = &[
    "n", "new", "q", "exit", "quit", "u", "uc", "ut", "workflow", "unlock", "sandbox",
];

/// Parse a REPL line as a skill-toggle command.
///
/// Returns `Some((skill_name, enabled))` when the line is exactly:
///   `/skillname`        → enable  (enabled = true)
///   `/skillname off`    → disable (enabled = false)
///
/// Returns `None` for any other input, including:
///   - lines not starting with `/`
///   - reserved built-in commands (`/n`, `/q`, `/u`, `/uc`, `/ut`, `/exit`)
///   - bare `/` with no name
///   - names containing `/` (path-like) or characters outside `[a-zA-Z0-9_-]`
///   - three or more whitespace-separated tokens
fn parse_skill_toggle(line: &str) -> Option<(String, bool)> {
    let rest = line.strip_prefix('/')?;
    // Bare slash
    if rest.is_empty() {
        return None;
    }

    // Split on whitespace: expect 1 or 2 tokens
    let mut tokens = rest.split_whitespace();
    let name = tokens.next()?;
    let modifier = tokens.next(); // "off" or absent
                                  // Reject any further tokens
    if tokens.next().is_some() {
        return None;
    }

    // Reserved built-in names
    if BUILTIN_PREFIXES.contains(&name) {
        return None;
    }

    // Validate name: alphanumeric, hyphen, underscore only
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }

    let enabled = match modifier {
        None => true,
        Some(m) if m.eq_ignore_ascii_case("off") => false,
        _ => return None, // unrecognised modifier
    };

    Some((name.to_string(), enabled))
}

#[cfg(test)]
mod tests {
    // Validates the signal+join mechanism used by end_turn.
    // Marked ignore because it mutates terminal state — run manually:
    //   cargo test -p gait-repl watcher_raw_mode -- --ignored
    #[test]
    #[ignore]
    fn watcher_raw_mode_off_after_teardown() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::runtime::Runtime;

        // multi-thread runtime is required for block_in_place
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let done = Arc::new(AtomicBool::new(false));
            let done2 = done.clone();
            let handle = tokio::task::spawn_blocking(move || {
                let _ = crossterm::terminal::enable_raw_mode();
                let _g = super::RawModeGuard;
                while !done2.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            });

            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert!(
                crossterm::terminal::is_raw_mode_enabled().unwrap_or(false),
                "watcher should have enabled raw mode"
            );

            // Simulate what end_turn does: signal then block_in_place join
            done.store(true, Ordering::SeqCst);
            tokio::task::block_in_place(|| {
                let _ = tokio::runtime::Handle::current().block_on(handle);
            });

            assert!(
                !crossterm::terminal::is_raw_mode_enabled().unwrap_or(true),
                "raw mode must be off after watcher join"
            );
        });
    }
}

#[cfg(test)]
mod skill_toggle_tests {
    use super::parse_skill_toggle;

    #[test]
    fn enable_bare_name() {
        assert_eq!(parse_skill_toggle("/tdd"), Some(("tdd".to_string(), true)));
    }

    #[test]
    fn enable_with_trailing_whitespace() {
        assert_eq!(
            parse_skill_toggle("/tdd  "),
            Some(("tdd".to_string(), true))
        );
    }

    #[test]
    fn disable_with_off() {
        assert_eq!(
            parse_skill_toggle("/tdd off"),
            Some(("tdd".to_string(), false))
        );
    }

    #[test]
    fn disable_with_off_uppercase() {
        assert_eq!(
            parse_skill_toggle("/tdd OFF"),
            Some(("tdd".to_string(), false))
        );
    }

    #[test]
    fn disable_with_extra_whitespace() {
        assert_eq!(
            parse_skill_toggle("/tdd   off"),
            Some(("tdd".to_string(), false))
        );
    }

    #[test]
    fn plain_message_returns_none() {
        assert_eq!(parse_skill_toggle("hello world"), None);
    }

    #[test]
    fn known_builtins_return_none() {
        assert_eq!(parse_skill_toggle("/n"), None);
        assert_eq!(parse_skill_toggle("/new"), None);
        assert_eq!(parse_skill_toggle("/q"), None);
        assert_eq!(parse_skill_toggle("/exit"), None);
        assert_eq!(parse_skill_toggle("/u"), None);
        assert_eq!(parse_skill_toggle("/uc"), None);
        assert_eq!(parse_skill_toggle("/ut"), None);
    }

    #[test]
    fn bare_slash_returns_none() {
        assert_eq!(parse_skill_toggle("/"), None);
    }

    #[test]
    fn invalid_chars_in_name_return_none() {
        assert_eq!(parse_skill_toggle("/foo/bar"), None);
        assert_eq!(parse_skill_toggle("/foo bar baz"), None);
    }

    #[test]
    fn hyphen_underscore_allowed() {
        assert_eq!(
            parse_skill_toggle("/my-skill"),
            Some(("my-skill".to_string(), true))
        );
        assert_eq!(
            parse_skill_toggle("/my_skill"),
            Some(("my_skill".to_string(), true))
        );
    }
}
