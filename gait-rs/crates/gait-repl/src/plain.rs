//! The plain REPL: a `reedline`-backed line editor and a `TurnSink` that
//! writes streaming text/tool notices to stderr and final results to
//! stdout — no rich-TUI rendering (no `termimad` markdown, no `syntect`
//! syntax highlighting, no ANSI cursor-motion or alternate-screen escape
//! sequences). Compiled in every REPL build, gated behind nothing; the
//! themed `chat::run_chat` REPL (`rich-tui` feature) is the alternative
//! front end for full `gait`.
//!
//! Session-control commands mirror the rich REPL's real command set
//! (`repl::commands::COMMANDS`) rather than inventing new names:
//! `/help`, `/info`, `/inference`, `/auth`, `/new`, `/undo`, `/sandbox`,
//! `/unlock`, `/skill`, and `/exit` (alias `/q`). `/skill` delegates to
//! `repl::skill`, the same load/unload/scan logic the rich REPL's
//! `repl::driver` dispatch uses — not a reimplementation.
use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;

use gait_core::embed::{user_turn_events, Diag, TurnSink, TurnSource, TurnStart};
use gait_core::proto::gait::log::v1::{
    self as pb, event, payload, turn_done, turn_result, PauseReason, RunResult, StreamItem,
};
use gait_core::proto::{events, wire};

use crate::repl::commands::{self as cmd, ReplCommand, ReplCommandId};
use crate::repl::{LineReader, ReedlineReader};
use crate::repl::prompt::GaitPrompt;

/// Filtered subset of `repl::commands::COMMANDS`: the session-control
/// commands the plain REPL supports. `gait-manual/manual.yaml`'s ungated
/// guides ("Getting started": /help /info; "Sessions": /new /undo /unlock;
/// "Inferences and providers": /inference /auth) all resolve here; `/sandbox`
/// and `/skill` are required per the plan's Global Constraints and "skills
/// stay in Min" respectively; `/exit` always.
pub const MIN_REPL_COMMANDS: &[ReplCommand] = &[
    pick(ReplCommandId::Help),
    pick(ReplCommandId::Info),
    pick(ReplCommandId::Inference),
    pick(ReplCommandId::Auth),
    pick(ReplCommandId::New),
    pick(ReplCommandId::Undo),
    pick(ReplCommandId::Sandbox),
    pick(ReplCommandId::Unlock),
    pick(ReplCommandId::Skill),
    pick(ReplCommandId::Exit),
];

const fn pick(id: ReplCommandId) -> ReplCommand {
    let commands = cmd::COMMANDS;
    let mut i = 0;
    while i < commands.len() {
        if commands[i].id as u8 == id as u8 {
            return commands[i];
        }
        i += 1;
    }
    panic!("MIN_REPL_COMMANDS: id not found in repl::commands::COMMANDS");
}

pub struct PlainReplArgs {
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub agent_name: Option<String>,
    pub session_id: Option<String>,
    pub model_override: Option<String>,
}

/// A session-control signal the source raises to end the current
/// `run_turn_source` drive. Mirrors `repl::driver::Control` — kept as its own
/// tiny type here rather than shared, since the rich REPL's `Control` lives
/// behind `repl::driver`, which is `rich-tui`-only.
enum Control {
    NewSession(String),
    Quit,
}

/// A mid-turn interaction the model raised, captured off the stream so
/// `PlainSource` can resolve it (read a reply, append it, respawn) once the
/// turn settles. Plain-text equivalent of `ui::state::Pending`.
enum Pending {
    None,
    Clarify(Vec<pb::ClarifyItem>),
    Inform(Vec<pb::InformItem>),
    Permission(Vec<pb::PermissionItem>),
}

/// Stateless proxy onto the real process stdout — cheap to construct fresh
/// per session/turn since `std::io::stdout()` is just a thin handle onto a
/// process-global, already-synchronized stream.
struct StdoutWriter;
impl Write for StdoutWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::stdout().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

/// Stateless proxy onto the real process stderr (see `StdoutWriter`).
struct StderrWriter;
impl Write for StderrWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::stderr().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

struct PlainState {
    control: Option<Control>,
    sandbox_on: bool,
    pending: Pending,
    /// Accumulated assistant text for the in-flight turn (used as the
    /// `assistant:` fallback when a turn completes without a structured
    /// `done` message).
    turn_text: String,
}

type Shared = Rc<RefCell<PlainState>>;

/// Entry point: the plain REPL's `chat::run_chat` equivalent. Builds a real
/// `Reedline` editor (no highlighter/theming — `reedline`/`crossterm` stay
/// unconditional dependencies regardless of `rich-tui`) and drives sessions
/// through `gait_turn_runner::run_turn_source`, writing to the real stdout/
/// stderr.
pub async fn run_plain_repl(args: PlainReplArgs) -> anyhow::Result<()> {
    let rl = reedline::Reedline::create();
    let reader: Box<dyn LineReader> = Box::new(ReedlineReader(rl));
    run_plain_loop(
        args,
        reader,
        || Box::new(StdoutWriter) as Box<dyn Write>,
        || Box::new(StderrWriter) as Box<dyn Write>,
    )
    .await
}

/// A `Write` handle onto a shared in-memory buffer, used by
/// `run_plain_script_fresh` to capture everything the plain REPL writes
/// (stdout+stderr combined, matching the acceptance test's "one transcript"
/// shape) across a real `run_plain_loop` drive. Not `#[cfg(test)]` — this
/// function is called from `tests/plain_repl_e2e.rs`, a separate crate that
/// only sees the library's normal (non-test) build.
#[cfg(feature = "dev-harness")]
struct CapturingWriter(Rc<RefCell<Vec<u8>>>);
#[cfg(feature = "dev-harness")]
impl Write for CapturingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Drives a scripted, mock-brain-backed conversation through the plain
/// REPL's *real* entry point (`run_plain_loop`, the same call
/// `run_plain_repl` makes) — a fresh git-inited temp gait-dir, a
/// `ScriptedReader` over `script`'s turn inputs, and `GAIT_BRAIN_EXE`
/// pointed at `mock_brain_exe` — and returns everything written to
/// stdout+stderr. Unlike `dev_harness::run_script_fresh` this never
/// constructs a `ReplBuilder`/`Theme`, so it's usable with `rich-tui` off;
/// see `tests/plain_repl_e2e.rs`.
#[cfg(feature = "dev-harness")]
pub async fn run_plain_script_fresh(
    script: &crate::turn_script::Script,
    mock_brain_exe: &std::path::Path,
) -> anyhow::Result<(tempfile::TempDir, String)> {
    let (ws, gait_dir, codebase) =
        crate::turn_script::setup_fresh_gait_dir_and_codebase(mock_brain_exe)?;
    crate::turn_script::write_numbered_frames(&gait_dir, &script.turns)?;

    let lines: Vec<&str> = script.turns.iter().map(|t| t.input.as_str()).collect();
    let reader: Box<dyn LineReader> = Box::new(crate::repl::ScriptedReader::lines(&lines));

    let buf = Rc::new(RefCell::new(Vec::<u8>::new()));
    let out_buf = buf.clone();
    let err_buf = buf.clone();
    let args = PlainReplArgs {
        gait_dir,
        codebase,
        agent_name: None,
        session_id: Some(gait_core::session::new_session_id()),
        model_override: None,
    };
    run_plain_loop(
        args,
        reader,
        move || Box::new(CapturingWriter(out_buf.clone())) as Box<dyn Write>,
        move || Box::new(CapturingWriter(err_buf.clone())) as Box<dyn Write>,
    )
    .await?;

    let output = String::from_utf8_lossy(&buf.borrow()).to_string();
    Ok((ws, output))
}

/// The reusable driver behind `run_plain_repl`: takes an injectable line
/// reader and writer factories (called fresh per session-loop iteration, so
/// a test can hand back clones of one shared in-memory buffer) so both the
/// real binary and tests exercise the exact same session loop.
async fn run_plain_loop(
    args: PlainReplArgs,
    reader: Box<dyn LineReader>,
    mk_out: impl Fn() -> Box<dyn Write>,
    mk_err: impl Fn() -> Box<dyn Write>,
) -> anyhow::Result<()> {
    let PlainReplArgs {
        gait_dir,
        codebase,
        agent_name,
        session_id,
        model_override,
    } = args;

    let agent_path: Option<PathBuf> = match agent_name.as_deref() {
        Some(n) => Some(gait_core::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_core::agent_frontmatter::MarkdownKind::Agent,
            n,
        )),
        None => {
            let default = gait_core::agent_frontmatter::resolve_markdown_context_path(
                &gait_dir,
                gait_core::agent_frontmatter::MarkdownKind::Agent,
                "default",
            );
            if default.exists() {
                Some(default)
            } else {
                None
            }
        }
    };

    let loaded_config = match gait_core::config::load_config_with(
        &gait_dir,
        gait_core::config::ConfigLoadPolicy::Strict,
    ) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(mk_err(), "[gait] {e}");
            std::process::exit(1);
        }
    };

    let mut session_id = session_id.unwrap_or_else(gait_core::session::new_session_id);
    let shared: Shared = Rc::new(RefCell::new(PlainState {
        control: None,
        sandbox_on: loaded_config.sandbox.on,
        pending: Pending::None,
        turn_text: String::new(),
    }));
    let repos = gait_core::git::Repos::production();
    let base_allow = loaded_config.allow.clone();
    let base_deny = loaded_config.deny.clone();

    // One reader for the process lifetime; `/n` only resets session-local
    // state, not the editor (the plain REPL has no themed history styling to
    // reset for).
    let reader = Rc::new(RefCell::new(reader));

    loop {
        let source = PlainSource {
            shared: shared.clone(),
            reader: reader.clone(),
            err: mk_err(),
            gait_dir: gait_dir.clone(),
            codebase: codebase.clone(),
            session_id: session_id.clone(),
            agent_path: agent_path.clone(),
            repos: repos.clone(),
            model_override: model_override.clone(),
            ran_turn: false,
        };
        let mut sink = PlainSink {
            shared: shared.clone(),
            out: mk_out(),
            err: mk_err(),
        };

        let run_id = format!("chat-{}", session_id);
        let outcome = gait_turn_runner::run_turn_source(
            gait_turn_runner::TurnRunOpts {
                run_id,
                session_id: session_id.clone(),
                continue_session: session_id.clone(),
                gait_dir: gait_dir.clone(),
                codebase: codebase.clone(),
                args: Vec::new(),
                sandbox_on: None,
                cascade_group_arg: None,
                cascade: None,
                profile: model_override.clone().map(|model| {
                    gait_turn_runner::TurnProfileOverride {
                        inference: Some(model),
                        explicit_inference_override: true,
                        session_updates: Vec::new(),
                        allow: base_allow.clone(),
                        deny: base_deny.clone(),
                        agent: None,
                        max_iter: None,
                        result_schema: None,
                        auto: None,
                    }
                }),
                fs_delegate: None,
                allow_intree: false,
            },
            Box::new(source),
            &mut sink,
        )
        .await?;
        debug_assert!(outcome.opened || outcome.completed);

        let control = shared.borrow_mut().control.take();
        match control {
            Some(Control::NewSession(id)) => {
                session_id = id;
                let _ = writeln!(mk_err(), "new session {}", session_id);
                continue;
            }
            Some(Control::Quit) | None => break,
        }
    }

    Ok(())
}

/// View-layer sink: writes streaming text/tool notices to stderr and the
/// finished turn's result to stdout. Plain text only — no ANSI cursor
/// motion, no alternate screen buffer, no markdown/syntax rendering.
struct PlainSink {
    shared: Shared,
    out: Box<dyn Write>,
    err: Box<dyn Write>,
}

impl TurnSink for PlainSink {
    fn item(&mut self, it: &StreamItem) {
        use gait_core::proto::gait::log::v1::stream_item::Item;
        match &it.item {
            Some(Item::Delta(d)) => {
                self.shared.borrow_mut().turn_text.push_str(&d.content);
                let _ = write!(self.err, "{}", d.content);
                let _ = self.err.flush();
            }
            Some(Item::Event(ev)) => self.handle_event(ev),
            None => {}
        }
    }

    fn result(&mut self, r: &RunResult) {
        let Some(turn) = r.turns.last() else {
            return;
        };
        match &turn.outcome {
            Some(turn_result::Outcome::Done(done)) => {
                let message = match &done.output {
                    Some(turn_done::Output::Structured(s)) if !s.message.is_empty() => {
                        s.message.clone()
                    }
                    _ => std::mem::take(&mut self.shared.borrow_mut().turn_text),
                };
                let _ = writeln!(self.out, "assistant: {}", message.trim());
                if let Some(turn_done::Output::Structured(s)) = &done.output {
                    if let Some(result) = s
                        .result
                        .as_ref()
                        .and_then(|v| serde_json::to_value(v).ok())
                        .and_then(|v| serde_json::to_string_pretty(&v).ok())
                    {
                        let _ = writeln!(self.out, "{result}");
                    }
                }
            }
            Some(turn_result::Outcome::Cancelled(_)) => {
                let _ = writeln!(self.err, "interrupted");
            }
            Some(turn_result::Outcome::Pause(pause))
                if pause.reason == PauseReason::SandboxUnlock as i32 =>
            {
                let _ = writeln!(
                    self.err,
                    "sandbox: item(s) need unlock — run: /unlock [once|session|global|agent]"
                );
            }
            Some(turn_result::Outcome::Pause(_)) => {
                // Clarify/Inform/Permission pauses are handled off the raw
                // events in `handle_event` below (Pending), not here.
            }
            Some(turn_result::Outcome::Failed(f)) => {
                let _ = writeln!(self.err, "[gait] turn failed: {}", f.message);
            }
            None => {}
        }
        self.shared.borrow_mut().turn_text.clear();
    }

    fn session_id(&mut self, _id: &str) {}

    fn diagnostic(&mut self, level: Diag, msg: &str) {
        if level == Diag::Error {
            let _ = writeln!(self.err, "[gait] {msg}");
        }
    }
}

impl PlainSink {
    fn handle_event(&mut self, ev: &pb::Event) {
        let Some(event::Body::Payload(p)) = &ev.body else {
            return;
        };
        match &p.kind {
            Some(payload::Kind::ToolCall(tc)) => {
                let _ = writeln!(self.err, "\n> {}", tc.name);
            }
            Some(payload::Kind::ToolResult(tr)) => {
                let ok = tr.process.as_ref().is_none_or(|pr| pr.exit_code == 0);
                let _ = writeln!(self.err, "  {}", if ok { "ok" } else { "failed" });
            }
            Some(payload::Kind::Error(e)) => {
                let _ = writeln!(self.err, "[gait] error: {}", e.message);
            }
            Some(payload::Kind::Permissions(pm)) => {
                self.shared.borrow_mut().pending = Pending::Permission(pm.items.clone());
            }
            Some(payload::Kind::Clarify(c)) => {
                self.shared.borrow_mut().pending = Pending::Clarify(c.items.clone());
            }
            Some(payload::Kind::Inform(inf)) => {
                self.shared.borrow_mut().pending = Pending::Inform(inf.items.clone());
            }
            _ => {}
        }
    }
}

/// Line reader + slash-command dispatch + interaction resolution. The plain
/// REPL's equivalent of `repl::driver::ReplSource`, without the themed
/// `ReplBuilder` it's built on — everything renders through the injected
/// `out`/`err` writers as plain text instead.
struct PlainSource {
    shared: Shared,
    reader: Rc<RefCell<Box<dyn LineReader>>>,
    err: Box<dyn Write>,
    gait_dir: PathBuf,
    codebase: PathBuf,
    session_id: String,
    agent_path: Option<PathBuf>,
    repos: gait_core::git::Repos,
    model_override: Option<String>,
    ran_turn: bool,
}

impl PlainSource {
    fn log_path(&self) -> PathBuf {
        gait_core::session::live::log_path(&self.gait_dir, &self.session_id)
    }

    fn agent_label(&self) -> String {
        self.agent_path
            .as_ref()
            .and_then(|p| p.file_stem().and_then(|s| s.to_str()))
            .unwrap_or("default")
            .to_string()
    }

    fn model_short(&self) -> String {
        if let Some(model) = &self.model_override {
            return crate::repl::status::short_model(model);
        }
        let cfg = gait_core::config::load_config(&self.gait_dir);
        let base = cfg.inference.unwrap_or_default();
        gait_core::inferences::load_inference(&self.gait_dir, &base)
            .ok()
            .map(|i| crate::repl::status::short_model(&i.model))
            .unwrap_or_else(|| "?".into())
    }

    fn eprintln(&mut self, text: &str) {
        let _ = writeln!(self.err, "{text}");
    }

    /// Resolve a pending Clarify/Inform/Permission interaction by reading one
    /// plain-text reply per item. Returns the event body to append and
    /// whether the turn should respawn (fold the reply and continue), or
    /// `None` if there was nothing pending.
    fn run_interaction(&mut self) -> Option<(event::Body, bool)> {
        let pending = std::mem::replace(&mut self.shared.borrow_mut().pending, Pending::None);
        match pending {
            Pending::None => None,
            Pending::Clarify(items) => {
                let mut replies = Vec::new();
                for item in &items {
                    self.eprintln(&item.content);
                    if !item.why.is_empty() {
                        self.eprintln(&format!("  why: {}", item.why));
                    }
                    let raw = self.read_line_raw();
                    replies.push(pb::ClarifyReplyItem {
                        id: item.id.clone(),
                        content: raw,
                        reasoning: None,
                    });
                }
                Some((events::clarify_reply(&replies), true))
            }
            Pending::Inform(items) => {
                let mut replies = Vec::new();
                for item in &items {
                    self.eprintln(&item.content);
                    let raw = self.read_line_raw();
                    let content = if raw.is_empty() { "understood".into() } else { raw };
                    replies.push(pb::InformReplyItem {
                        id: item.id.clone(),
                        content,
                    });
                }
                Some((events::inform_reply(&replies), true))
            }
            Pending::Permission(items) => {
                let mut reply_items = Vec::new();
                let mut cancelled = false;
                for item in &items {
                    self.eprintln(&format!("permission: {}", item.cmd));
                    if !item.reason.is_empty() {
                        self.eprintln(&format!("  {}", item.reason));
                    }
                    for line in crate::ui::permission::preview_block(item.preview.as_deref()) {
                        self.eprintln(&line);
                    }
                    self.eprintln(
                        "  0 = all, number = once, s = session, m = mode, r = reject, rs = reject session",
                    );
                    let raw = self.read_line_raw();
                    let choice = if cancelled {
                        gait_core::permissions::PermissionChoice::Deny
                    } else {
                        match crate::ui::permission::parse_permission_response(
                            &raw,
                            &item.patterns,
                        ) {
                            Some(c) => c,
                            None => {
                                cancelled = true;
                                gait_core::permissions::PermissionChoice::Deny
                            }
                        }
                    };
                    if let Err(e) = gait_core::permissions::save_choice(
                        &choice,
                        &self.gait_dir,
                        &self.session_id,
                        gait_ops::permission::resolve_active_mode_name(&self.gait_dir, &self.session_id).as_deref(),
                    ) {
                        self.eprintln(&format!("[gait] warning: could not save permission: {e}"));
                    }
                    reply_items.push(match &choice {
                        gait_core::permissions::PermissionChoice::Once(p) => {
                            pb::PermissionsReplyItem {
                                id: item.id.clone(),
                                decision: pb::Decision::Allow as i32,
                                text: Some(p.clone()),
                                scope: pb::Scope::Once as i32,
                            }
                        }
                        gait_core::permissions::PermissionChoice::Session(p) => {
                            pb::PermissionsReplyItem {
                                id: item.id.clone(),
                                decision: pb::Decision::Allow as i32,
                                text: Some(p.clone()),
                                scope: pb::Scope::Session as i32,
                            }
                        }
                        gait_core::permissions::PermissionChoice::Mode(p) => {
                            pb::PermissionsReplyItem {
                                id: item.id.clone(),
                                decision: pb::Decision::Allow as i32,
                                text: Some(p.clone()),
                                scope: pb::Scope::Mode as i32,
                            }
                        }
                        gait_core::permissions::PermissionChoice::Global(p) => {
                            pb::PermissionsReplyItem {
                                id: item.id.clone(),
                                decision: pb::Decision::Allow as i32,
                                text: Some(p.clone()),
                                scope: pb::Scope::Global as i32,
                            }
                        }
                        gait_core::permissions::PermissionChoice::Deny => {
                            pb::PermissionsReplyItem {
                                id: item.id.clone(),
                                decision: pb::Decision::Deny as i32,
                                text: None,
                                scope: pb::Scope::Unspecified as i32,
                            }
                        }
                        gait_core::permissions::PermissionChoice::DenySession(p) => {
                            pb::PermissionsReplyItem {
                                id: item.id.clone(),
                                decision: pb::Decision::Deny as i32,
                                text: Some(p.clone()),
                                scope: pb::Scope::Session as i32,
                            }
                        }
                    });
                }
                Some((events::permissions_reply(&reply_items), !cancelled))
            }
        }
    }

    fn read_line_raw(&mut self) -> String {
        let prompt = reedline::DefaultPrompt::default();
        match self.reader.borrow_mut().read_line(&prompt) {
            Ok(reedline::Signal::Success(s)) => s.trim().to_string(),
            _ => String::new(),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl TurnSource for PlainSource {
    async fn next(&mut self) -> Option<TurnStart> {
        if self.ran_turn {
            if let Some((body, respawn)) = self.run_interaction() {
                if wire::append(&self.log_path(), body).is_ok() && respawn {
                    return Some(TurnStart::Resume);
                }
            }
        }
        self.ran_turn = false;

        loop {
            let prompt = GaitPrompt {
                agent: self.agent_label(),
                model: self.model_short(),
                branch: String::new(),
                open_node: None,
                indicator: "> ".to_string(),
            };
            let line = match self.reader.borrow_mut().read_line(&prompt) {
                Ok(reedline::Signal::Success(buf)) => buf.trim().to_string(),
                Ok(reedline::Signal::CtrlD) => {
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
                Ok(reedline::Signal::CtrlC) => continue,
                Ok(_) => {
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
                Err(_) => {
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
            };
            if line.is_empty() {
                continue;
            }

            let parsed = cmd::parse(&line);

            if matches!(
                parsed,
                Some(cmd::ParsedCommand {
                    id: ReplCommandId::Help,
                    ..
                })
            ) {
                self.eprintln(&min_help_text());
                continue;
            }

            if matches!(
                parsed,
                Some(cmd::ParsedCommand {
                    id: ReplCommandId::Info,
                    ..
                })
            ) {
                let cfg = gait_core::config::load_config(&self.gait_dir);
                let model = cfg.inference.as_deref().unwrap_or("(none)");
                self.eprintln(&format!(
                    "inference: {}\nsession: {}\ngait-dir: {}\ncodebase: {}",
                    model,
                    self.session_id,
                    self.gait_dir.display(),
                    self.codebase.display(),
                ));
                continue;
            }

            if let Some(cmd::ParsedCommand {
                id: ReplCommandId::Auth,
                args,
            }) = parsed
            {
                self.dispatch_auth(args);
                continue;
            }

            if let Some(cmd::ParsedCommand {
                id: ReplCommandId::Inference,
                args,
            }) = parsed
            {
                if let Some(update) = self.dispatch_inference(args) {
                    return Some(update);
                }
                continue;
            }

            if let Some(cmd::ParsedCommand {
                id: ReplCommandId::Sandbox,
                args,
            }) = parsed
            {
                if !args.is_empty() && args != "on" && args != "off" {
                    self.eprintln("[gait] usage: /sandbox [off|on]");
                    continue;
                }
                let on = args != "off";
                self.shared.borrow_mut().sandbox_on = on;
                self.eprintln(if on { "sandbox enabled" } else { "sandbox disabled" });
                continue;
            }

            if let Some(cmd::ParsedCommand {
                id: ReplCommandId::Unlock,
                args: scope,
            }) = parsed
            {
                self.dispatch_unlock(scope);
                continue;
            }

            if let Some(cmd::ParsedCommand {
                id: ReplCommandId::Skill,
                args: rest,
            }) = parsed
            {
                self.dispatch_skill(rest);
                continue;
            }

            if let Some(cmd::ParsedCommand {
                id: ReplCommandId::Undo,
                args,
            }) = parsed
            {
                self.dispatch_undo(args);
                continue;
            }

            match parsed.map(|c| c.id) {
                Some(ReplCommandId::New) => {
                    self.shared.borrow_mut().control =
                        Some(Control::NewSession(gait_core::session::new_session_id()));
                    return None;
                }
                Some(ReplCommandId::Exit) => {
                    self.shared.borrow_mut().control = Some(Control::Quit);
                    return None;
                }
                _ => {}
            }

            // `cmd::parse` matches against the FULL `repl::commands::COMMANDS`
            // table, not just `MIN_REPL_COMMANDS` — so a command outside this
            // build's supported set (e.g. `/config`, `/mode`, `/shell`,
            // `/workflow`, `/undo-code`, `/undo-transcript`) still parses
            // successfully here. Every id actually supported by the plain
            // REPL is handled above and already `continue`d or `return`ed by
            // this point, so any `Some(_)` still reaching here is one of
            // those unsupported commands — catch it before it falls through
            // to being sent to the model as literal prompt text.
            if parsed.is_some() {
                self.eprintln("[gait] not available in this build");
                continue;
            }

            // Unified expansion (identical to the rich REPL and the `gait-min`
            // CLI path): $(cmd)/$(N), @path. The plain REPL has no `!cmd`
            // shell-history recorder, so `$(N)` history recall always misses
            // (there is no `sh.jsonl` writer to populate it); `>(N)` quote-reply
            // has no prior-turn blocks to draw on here, so it is passed empty.
            let expanded = gait_core::expand::expand_input(
                &line,
                &gait_core::expand::ExpandCtx {
                    codebase: &self.codebase,
                    history: &self.gait_dir.join("sh.jsonl"),
                    last_blocks: &[],
                },
            );
            let bodies = user_turn_events(&self.repos, &self.codebase, expanded)
                .into_iter()
                .map(|b| (b, None).into())
                .collect();
            self.ran_turn = true;
            return Some(TurnStart::Input { bodies });
        }
    }
}

impl PlainSource {
    fn dispatch_auth(&mut self, args: &str) {
        let mut parts = args.splitn(2, char::is_whitespace);
        let action = parts.next().unwrap_or("").trim();
        let rest = parts.next().unwrap_or("").trim();
        match action {
            "" | "list" => match gait_ops::auth::list() {
                Ok(credentials) if credentials.is_empty() => {
                    self.eprintln("No credentials configured.")
                }
                Ok(credentials) => {
                    for (provider, display) in credentials {
                        self.eprintln(&format!("  {provider}: {display}"));
                    }
                }
                Err(e) => self.eprintln(&format!("[gait] /auth: {e}")),
            },
            "status" => match gait_ops::auth::status(rest) {
                Ok(status) => self.eprintln(&format!(
                    "{rest}: {}",
                    status.unwrap_or_else(|| "not configured".to_string())
                )),
                Err(e) => self.eprintln(&format!("[gait] /auth: {e}")),
            },
            "unlink" => match gait_ops::auth::unlink(rest) {
                Ok(out) => self.eprintln(&format!(
                    "Unlinked {} external credentials",
                    out.provider
                )),
                Err(e) => self.eprintln(&format!("[gait] /auth: {e}")),
            },
            "link" => {
                let mut it = rest.splitn(2, char::is_whitespace);
                let provider = it.next().unwrap_or("").trim();
                let source = it.next().unwrap_or("").trim();
                if provider.is_empty() || source.is_empty() {
                    self.eprintln("[gait] usage: /auth link <provider> <source> [--home <path>]");
                    return;
                }
                match gait_ops::auth::link(provider, source, None) {
                    Ok(out) => self.eprintln(&format!(
                        "Linked {} credentials ({})",
                        out.provider, out.display
                    )),
                    Err(e) => self.eprintln(&format!("[gait] /auth: {e}")),
                }
            }
            _ => self.eprintln(
                "[gait] usage: /auth [list|status <provider>|link <provider> <source>|unlink <provider>]",
            ),
        }
    }

    fn dispatch_inference(&mut self, arg: &str) -> Option<TurnStart> {
        if arg.is_empty() {
            for name in gait_ops::inference::list(&self.gait_dir) {
                self.eprintln(&format!("  {name}"));
            }
            return None;
        }
        if let Some(name) = arg.strip_prefix("show ") {
            match gait_ops::inference::show(&self.gait_dir, name.trim()) {
                Ok(i) => self.eprintln(&format!("  {}\t{}", i.name, i.model)),
                Err(e) => self.eprintln(&format!("[gait] /inference: {e}")),
            }
            return None;
        }
        if let Some(args) = arg.strip_prefix("add ") {
            let parts: Vec<&str> = args.splitn(3, char::is_whitespace).collect();
            if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
                self.eprintln(
                    "[gait] /inference add <name> <provider/id> <context-window-tokens>",
                );
                return None;
            }
            let Ok(compaction_threshold_tokens) = parts[2].parse() else {
                self.eprintln("[gait] context-window-tokens must be a positive integer");
                return None;
            };
            match gait_ops::inference::add(&self.gait_dir, parts[0], parts[1], compaction_threshold_tokens)
            {
                Ok(i) => self.eprintln(&format!("  added inference '{}' -> model {}", i.name, i.model)),
                Err(e) => self.eprintln(&format!("[gait] /inference: {e}")),
            }
            return None;
        }
        match gait_ops::inference::resolve_selection(&self.gait_dir, arg) {
            Ok(selection) => {
                self.eprintln(&format!("inference set to {arg}"));
                Some(TurnStart::SessionUpdate(vec![selection.event_body]))
            }
            Err(e) => {
                self.eprintln(&format!("[gait] /inference: {e}"));
                None
            }
        }
    }

    fn dispatch_unlock(&mut self, scope: &str) {
        let scope = if scope.is_empty() { "once" } else { scope };
        let op_scope = match scope {
            "once" => gait_ops::permission::GrantScope::Once,
            "session" => gait_ops::permission::GrantScope::Session,
            "global" => gait_ops::permission::GrantScope::Global,
            "agent" => gait_ops::permission::GrantScope::Agent,
            _ => {
                self.eprintln("[gait] usage: /unlock [once|session|global|agent]");
                return;
            }
        };
        match gait_ops::permission::unlock(gait_ops::permission::UnlockParams {
            gait_dir: self.gait_dir.clone(),
            session_id: self.session_id.clone(),
            scope: op_scope,
            agent: None,
        }) {
            Ok(outcome) => {
                for w in &outcome.warnings {
                    self.eprintln(&format!("[gait] unlock: {w}"));
                }
            }
            Err(e) => self.eprintln(&format!(
                "[gait] unlock failed: {e} — check: gait status {} --gait-dir {}",
                self.session_id,
                self.gait_dir.display()
            )),
        }
    }

    fn dispatch_skill(&mut self, rest: &str) {
        if rest.is_empty() {
            self.eprintln(&crate::repl::skill::list_text(&self.gait_dir, &self.session_id));
            return;
        }
        let mut parts = rest.splitn(2, char::is_whitespace);
        let verb = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("").trim();
        if let Err(msg) = crate::repl::skill::load_or_unload(&self.gait_dir, &self.session_id, verb, name)
        {
            self.eprintln(&format!("[gait] {msg}"));
        }
    }

    fn dispatch_undo(&mut self, arg: &str) {
        if arg.is_empty() {
            match gait_ops::session::undo(gait_ops::session::UndoParams {
                gait_dir: self.gait_dir.clone(),
                session_id: self.session_id.clone(),
                turn: None,
                code: false,
                transcript: false,
                list: true,
            }) {
                Ok(outcome) => {
                    for t in &outcome.turns {
                        let mark = t.checkpoint_short.as_deref().unwrap_or("--------");
                        self.eprintln(&format!("turn {}\t[{}]\t{}", t.turn, mark, t.summary));
                    }
                }
                Err(e) => self.eprintln(&format!("[undo] {e}")),
            }
            return;
        }
        match arg.parse::<u32>() {
            Ok(n) => match gait_ops::session::undo(gait_ops::session::UndoParams {
                gait_dir: self.gait_dir.clone(),
                session_id: self.session_id.clone(),
                turn: Some(n),
                code: true,
                transcript: true,
                list: false,
            }) {
                Ok(_) => self.eprintln(&format!("[undo] restored both to turn {n}")),
                Err(_) => self.eprintln(&format!(
                    "[undo] failed — is there a turn {n} for this session?"
                )),
            },
            Err(_) => self.eprintln("usage: /undo [N]"),
        }
    }
}

fn min_help_text() -> String {
    use std::fmt::Write as _;
    let mut out = String::from("Commands:\n");
    for c in MIN_REPL_COMMANDS {
        let aliases = if c.aliases.is_empty() {
            String::new()
        } else {
            format!(" ({})", c.aliases.join(", "))
        };
        let _ = writeln!(out, "  {}{}  — {}", c.slug, aliases, c.desc);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_core::proto::envelope;

    fn done_turn_result(turn_id: &str) -> pb::TurnResult {
        pb::TurnResult {
            turn_id: turn_id.into(),
            outcome: Some(turn_result::Outcome::Done(pb::TurnDone {
                output: Some(turn_done::Output::Structured(pb::StructuredDone {
                    message: String::new(),
                    result: None,
                })),
            })),
            ..Default::default()
        }
    }

    /// Feeds `lines` through a `ScriptedReader` and returns everything the
    /// plain REPL would have written to stdout+stderr, for the acceptance
    /// check: real content renders, but no rich-TUI control codes ever
    /// appear. Exercises the actual `PlainSource` command-dispatch path
    /// (`/q` really raises `Control::Quit` through `cmd::parse`) and the
    /// actual `PlainSink::item`/`result` methods against real proto
    /// `StreamItem`/`RunResult` values for the "hello" turn — this crate's
    /// unit tests don't spin up a real brain subprocess (that's
    /// `dev_harness`'s job, gated behind `rich-tui`), so the turn's stream is
    /// supplied directly rather than round-tripped through
    /// `gait_turn_runner::run_turn_source`.
    /// A `Write` handle onto a shared in-memory buffer — several of these can
    /// point at the same underlying `Vec<u8>`, which is how the test hands
    /// both `PlainSource` and `PlainSink` a writer while keeping a handle of
    /// its own to read the combined transcript back afterward.
    struct SharedBuf(Rc<RefCell<Vec<u8>>>);
    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    async fn run_plain_script(lines: &[&str]) -> String {
        let buf = Rc::new(RefCell::new(Vec::<u8>::new()));
        let reader = Rc::new(RefCell::new(
            Box::new(crate::repl::ScriptedReader::lines(lines)) as Box<dyn LineReader>,
        ));
        let shared: Shared = Rc::new(RefCell::new(PlainState {
            control: None,
            sandbox_on: true,
            pending: Pending::None,
            turn_text: String::new(),
        }));
        let tmp = tempfile::tempdir().unwrap();
        let mut source = PlainSource {
            shared: shared.clone(),
            reader,
            err: Box::new(SharedBuf(buf.clone())),
            gait_dir: tmp.path().to_path_buf(),
            codebase: tmp.path().to_path_buf(),
            session_id: "s0000001".to_string(),
            agent_path: None,
            repos: gait_core::git::Repos::production(),
            model_override: None,
            ran_turn: false,
        };
        let mut sink = PlainSink {
            shared: shared.clone(),
            out: Box::new(SharedBuf(buf.clone())),
            err: Box::new(SharedBuf(buf.clone())),
        };

        // Drive the real dispatch loop line by line. A non-command line
        // ("hello") is where `PlainSource::next()` would hand back
        // `TurnStart::Input` for `run_turn_source` to drive for real; here we
        // simulate that turn's stream directly against the real `PlainSink`.
        loop {
            match source.next().await {
                Some(TurnStart::Input { .. }) => {
                    sink.item(&StreamItem {
                        item: Some(pb::stream_item::Item::Delta(pb::TextDelta {
                            content: "hi there\n".into(),
                        })),
                    });
                    let ev = envelope::mint(
                        events::turn_result(done_turn_result("t1")),
                        None,
                        envelope::EventScope::Turn { run: 0, turn: 0 },
                    );
                    sink.item(&StreamItem {
                        item: Some(pb::stream_item::Item::Event(ev)),
                    });
                    sink.result(&RunResult {
                        run_id: "r1".into(),
                        turns: vec![done_turn_result("t1")],
                        ..Default::default()
                    });
                    source.ran_turn = false; // no pending interaction to fold
                }
                Some(TurnStart::SessionUpdate(_)) | Some(TurnStart::Resume) => continue,
                None => break,
            }
        }

        let text = String::from_utf8_lossy(&buf.borrow()).to_string();
        text
    }

    #[tokio::test]
    async fn plain_repl_streams_text_without_tui_control_codes() {
        let output = run_plain_script(&["hello", "/q"]).await;
        assert!(output.contains("assistant"), "got: {output:?}");
        assert!(!output.contains("\x1b[2J"));
        assert!(!output.contains("\x1b[?1049h")); // alternate screen buffer, themed-TUI-only
    }

    #[test]
    fn min_repl_commands_covers_every_ungated_manual_guide() {
        let slugs: Vec<_> = MIN_REPL_COMMANDS.iter().map(|c| c.slug).collect();
        for s in [
            "/help", "/info", "/new", "/undo", "/unlock", "/inference", "/auth", "/sandbox",
            "/skill", "/exit",
        ] {
            assert!(slugs.contains(&s), "MIN_REPL_COMMANDS missing {s}");
        }
    }

    #[test]
    fn help_output_lists_min_commands_only() {
        let text = min_help_text();
        assert!(text.contains("/help"));
        assert!(text.contains("/skill"));
        // /workflow is not in MIN_REPL_COMMANDS (Task 5's job to gate).
        assert!(!text.contains("/workflow"));
    }
}
