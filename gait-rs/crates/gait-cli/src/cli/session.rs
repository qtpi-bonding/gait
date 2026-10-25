//! Session lifecycle + inspection commands.
//!
//! All logic lives in `gait_ops::session`. The functions here are thin
//! adapters: map clap `Args` → op params → call op → render outcome/error
//! → return a `CliError` (`cli::error`) on failure. The CLI keeps the 3-state
//! consent flow (`confirm_action -> Confirmed::{Yes, No, RequiresYes}`) for the
//! bulk persist/discard prompts; the op itself takes a `confirmed: bool` and
//! refuses (`OpError::ConfirmationRequired`) on `false`.
use crate::cli::error::{CliError, CliResult};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct PersistArgs {
    #[arg(conflicts_with = "all")]
    pub session_id: Option<String>,
    #[arg(long, conflicts_with = "session_id")]
    pub all: bool,
    #[arg(long, short = 'y')]
    pub yes: bool,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    /// Pretty-print the JSON result. Output is always JSON; this only
    /// controls indentation.
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct DiscardArgs {
    #[arg(conflicts_with = "all")]
    pub session_id: Option<String>,
    #[arg(long, conflicts_with = "session_id")]
    pub all: bool,
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Required alongside --all: --yes alone only skips the interactive
    /// y/N prompt, it does not acknowledge a bulk discard. Without --force,
    /// `discard --all` prints the dry-run list of what it WOULD discard and
    /// refuses to proceed, no matter how --yes is set. (2026-09-08:
    /// non-interactive callers were passing a blanket --yes and discarding
    /// 100+ sessions unattended -- this makes that a second, distinct,
    /// deliberate opt-in.)
    #[arg(long)]
    pub force: bool,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct StopArgs {
    pub session_id: String,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

enum Confirmed {
    Yes,
    No,
    RequiresYes,
}

/// Gate a mutating action. `--yes` (or `--json`, folded into `yes` by the caller)
/// short-circuits. On a TTY, prompt y/N. Non-TTY without `yes` => RequiresYes.
fn confirm_action(prompt: &str, yes: bool) -> Confirmed {
    use std::io::{IsTerminal, Write};
    if yes {
        return Confirmed::Yes;
    }
    if !std::io::stdin().is_terminal() {
        return Confirmed::RequiresYes;
    }
    eprint!("{} [y/N] ", prompt);
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Confirmed::No;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Confirmed::Yes,
        _ => Confirmed::No,
    }
}

// -----------------------------------------------------------------------------
// Single-session persist / discard (plan Step 5: route through a one-element plan)
// -----------------------------------------------------------------------------

pub fn cmd_persist(args: PersistArgs) -> CliResult {
    // `--json` used to double as an implicit `--yes` for scripted callers.
    // Output is unconditionally JSON now, so that shortcut is gone — a
    // non-interactive caller must pass `--yes` explicitly (or rely on
    // `confirm_action`'s existing non-TTY `RequiresYes` gate).
    if args.all {
        return run_bulk(
            gait_ops::session::BulkVerb::Persist,
            &args.gait_dir,
            args.yes,
            true, // force is meaningless for Persist (reversible; bulk_act ignores it)
            args.pretty,
        );
    }
    let Some(id) = args.session_id.as_deref() else {
        return Err(CliError::usage("[gait] error: specify a session id or --all"));
    };
    // One-element plan: render it, then call act(plan, confirmed).
    let plan = match gait_ops::session::classify_bulk(
        &args.gait_dir,
        gait_ops::session::BulkVerb::Persist,
    ) {
        Ok(p) => p,
        Err(e) => return Err(CliError::failed_with("[gait] error", e)),
    };
    if !plan.eligible.iter().any(|s| s == id) {
        return Err(CliError::invalid_input(format!(
            "[gait] error: session {} is not eligible to persist",
            id
        )));
    }
    let one: Vec<String> = vec![id.to_string()];
    let plan = gait_ops::session::BulkPlan {
        verb: gait_ops::session::BulkVerb::Persist,
        eligible: one,
        skipped: plan.skipped,
    };
    match gait_ops::session::bulk_act(&args.gait_dir, &plan, true, true) {
        Ok(outcome) => {
            println!(
                "{}",
                gait_ops::render_json(
                    &serde_json::json!({"session_id": id, "action": "persisted"}),
                    args.pretty
                )
            );
            for (wid, w) in &outcome.warnings {
                eprintln!("[gait] warning: session {}: {}", wid, w);
            }
            Ok(())
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

pub fn cmd_discard(args: DiscardArgs) -> CliResult {
    if args.all {
        return run_bulk(
            gait_ops::session::BulkVerb::Discard,
            &args.gait_dir,
            args.yes,
            args.force,
            args.pretty,
        );
    }
    let Some(id) = args.session_id.as_deref() else {
        return Err(CliError::usage("[gait] error: specify a session id or --all"));
    };
    // Per-session discard: route through the op's confirmation gate so the
    // 3-state UX (Yes / No / RequiresYes) still applies.
    match confirm_action(
        &format!("discard session {id} permanently (no merge, unrecoverable)"),
        args.yes,
    ) {
        Confirmed::Yes => {}
        Confirmed::No => {
            eprintln!("[gait] aborted");
            return Ok(());
        }
        Confirmed::RequiresYes => {
            return Err(CliError::usage(
                "[gait] error: refusing to discard non-interactively without --yes",
            ));
        }
    }
    let plan = gait_ops::session::BulkPlan {
        verb: gait_ops::session::BulkVerb::Discard,
        eligible: vec![id.to_string()],
        skipped: Vec::new(),
    };
    // Single-session discard: not gated on --force (bulk_act only requires
    // it for eligible.len() > 1) — the interactive/--yes confirm above is
    // this path's whole gate, same as before.
    if let Err(e) = gait_ops::session::bulk_act(&args.gait_dir, &plan, true, true) {
        return Err(CliError::failed_with("[gait] error", e));
    }
    println!(
        "{}",
        gait_ops::render_json(
            &serde_json::json!({"session_id": id, "action": "discarded"}),
            args.pretty
        )
    );
    Ok(())
}

// -----------------------------------------------------------------------------
// Bulk persist/discard with the 3-state consent flow
// -----------------------------------------------------------------------------

/// Bulk persist/discard over every idle, not-running session. Output is
/// always JSON on stdout (`pretty` only controls indentation); the
/// interactive y/N confirmation prompt and warning/failure diagnostics stay
/// plain text on stderr, per the existing `[gait] ...` diagnostic convention
/// (`gait_core::embed::CliSink::diagnostic`) — that split is about stdout
/// *results* vs. stderr *status/log lines*, not an exception to it.
fn run_bulk(
    verb: gait_ops::session::BulkVerb,
    gait_dir: &std::path::Path,
    yes: bool,
    force: bool,
    pretty: bool,
) -> CliResult {
    let plan = match gait_ops::session::classify_bulk(gait_dir, verb) {
        Ok(p) => p,
        Err(e) => return Err(CliError::failed_with("[gait] error", e)),
    };

    // The preview is itself a real result (what will happen) — always JSON.
    // This IS the dry-run listing: printed unconditionally, before any
    // confirmation or force check, so a caller sees exactly what would be
    // discarded even when the run below refuses to proceed.
    println!("{}", plan.to_json(pretty));

    if plan.eligible.is_empty() {
        return Ok(());
    }

    // A bulk (>1) discard needs --force, on top of --yes/confirmation — a
    // single blanket --yes must not be enough to wipe out many sessions at
    // once. Checked here (before the confirm prompt) so a non-interactive
    // caller gets this specific, actionable error instead of the generic
    // "refusing ... without --yes" one.
    if verb == gait_ops::session::BulkVerb::Discard && plan.eligible.len() > 1 && !force {
        return Err(CliError::usage(format!(
            "[gait] error: refusing to discard {} sessions without --force \
             (see the eligible list printed above; re-run with --force once reviewed)",
            plan.eligible.len()
        )));
    }

    // 3-state consent: Yes / No / RequiresYes. `No` aborts quietly and returns
    // Ok; `RequiresYes` (non-interactive without --yes) is a usage error.
    let label = match verb {
        gait_ops::session::BulkVerb::Persist => "persist",
        gait_ops::session::BulkVerb::Discard => "discard",
    };
    match confirm_action(
        &format!("{} {} session(s)?", label, plan.eligible.len()),
        yes,
    ) {
        Confirmed::Yes => {}
        Confirmed::No => {
            eprintln!("[gait] aborted");
            return Ok(());
        }
        Confirmed::RequiresYes => {
            return Err(CliError::usage(format!(
                "[gait] error: refusing to {} --all non-interactively without --yes",
                label
            )));
        }
    }

    let outcome = match gait_ops::session::bulk_act(gait_dir, &plan, true, force) {
        Ok(o) => o,
        Err(e) => return Err(CliError::failed_with("[gait] error", e)),
    };

    println!("{}", outcome.to_json(pretty));
    for (id, e) in &outcome.failed {
        eprintln!("  failed {}: {}", id, e);
    }
    for (id, w) in &outcome.warnings {
        eprintln!("[gait] warning: session {}: {}", id, w);
    }
    Ok(())
}

pub fn cmd_stop(_args: StopArgs) -> CliResult {
    Err(CliError::failed("[gait] error: stop not yet implemented"))
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct UndoArgs {
    pub session_id: String,
    #[arg(long)]
    pub turn: Option<u32>,
    #[arg(long)]
    pub code: bool,
    #[arg(long)]
    pub transcript: bool,
    #[arg(long)]
    pub list: bool,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

pub fn cmd_undo(args: UndoArgs) -> CliResult {
    use gait_ops::session::{undo as op_undo, UndoParams};
    let pretty = args.pretty;
    let params = UndoParams {
        gait_dir: args.gait_dir,
        session_id: args.session_id,
        turn: args.turn,
        code: args.code,
        transcript: args.transcript,
        list: args.list,
    };
    let outcome = match op_undo(params) {
        Ok(o) => o,
        Err(e) => return Err(CliError::failed_with("[gait] error", e)),
    };
    println!("{}", outcome.to_json(pretty));
    Ok(())
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct SessionsArgs {
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
    /// Show only the N most-recently-updated sessions.
    #[arg(long)]
    pub last: Option<usize>,
}

pub fn cmd_sessions(args: SessionsArgs) -> CliResult {
    match gait_ops::session::list(&args.gait_dir, args.last) {
        Ok(out) => {
            println!("{}", out.to_json(args.pretty));
            Ok(())
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct PsArgs {
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

pub fn cmd_ps(args: PsArgs) -> CliResult {
    match gait_ops::session::ps(&args.gait_dir) {
        Ok(out) => {
            println!("{}", out.to_json(args.pretty));
            Ok(())
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct StatsArgs {
    pub session_id: String,
    #[arg(long)]
    pub pretty: bool,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
}

pub fn cmd_stats(args: StatsArgs) -> CliResult {
    match gait_ops::session::stats(&args.gait_dir, &args.session_id) {
        Ok(u) => {
            println!("{}", u.to_json(args.pretty));
            Ok(())
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct LogArgs {
    pub session_id: String,
    #[arg(long)]
    pub pretty: bool,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
}

/// The last completed run's outcome (done message, checkpoint, files/tools
/// touched) — a quick glance without reading the full event stream.
pub fn cmd_log(args: LogArgs) -> CliResult {
    match gait_ops::session::summary(&args.gait_dir, &args.session_id) {
        Ok(out) => {
            println!("{}", out.to_json(args.pretty));
            Ok(())
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

// -----------------------------------------------------------------------------
// allow / deny / unlock / load / unload / reply — NOT part of Task 3 (permission/
// extension domains, Tasks 4-5). Restored verbatim; adapters land in later tasks.
// -----------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct AllowArgs {
    pub session_id: String,
    #[arg(long)]
    pub pattern: Option<String>,
    #[arg(long, value_name = "SCOPE")]
    pub scope: Option<String>,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

pub fn cmd_allow(args: AllowArgs) -> CliResult {
    use gait_ops::permission::{allow as op_allow, AllowParams, GrantScope};

    let scope = match args.scope.as_deref() {
        Some("global") => GrantScope::Global,
        Some("mode") => GrantScope::Mode,
        Some("session") => GrantScope::Session,
        Some("once") | None => GrantScope::Once,
        Some("agent") => {
            return Err(CliError::invalid_input(
                "[gait] error: --scope agent is no longer supported for allow; use --scope mode",
            ));
        }
        Some(other) => {
            return Err(CliError::invalid_input(format!(
                "[gait] error: unknown --scope '{}' (expected global|mode|session|once)",
                other
            )));
        }
    };

    let params = AllowParams {
        scope,
        pattern: args.pattern,
    };

    match op_allow(&args.gait_dir, &args.session_id, params) {
        Ok(out) => {
            println!(
                "{}",
                gait_ops::render_json(
                    &serde_json::json!({
                        "session_id": args.session_id,
                        "scope": format!("{:?}", out.scope).to_lowercase(),
                        "approved_count": out.reply_items.len(),
                    }),
                    args.pretty
                )
            );
            Ok(())
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct DenyArgs {
    pub session_id: String,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

pub fn cmd_deny(args: DenyArgs) -> CliResult {
    use gait_ops::permission::deny as op_deny;

    match op_deny(&args.gait_dir, &args.session_id) {
        Ok(out) => {
            println!(
                "{}",
                gait_ops::render_json(
                    &serde_json::json!({
                        "session_id": args.session_id,
                        "denied_count": out.reply_items.len(),
                    }),
                    args.pretty
                )
            );
            Ok(())
        }
        Err(e) if matches!(gait_ops::OpError::find(&e), Some(gait_ops::OpError::NotFound(_))) => {
            Err(CliError::not_found(format!(
                "[gait] no pending permissions for session {}",
                args.session_id
            )))
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct UnlockArgs {
    pub session_id: String,
    /// Grant scope: once (default, log only) | session (log only) | global (→ gait.jsonc) | agent (→ agent file)
    #[arg(long, default_value = "once")]
    pub scope: String,
    /// Agent name for agent-scoped grants (resolves to agents/<name>.md)
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

pub fn cmd_unlock(args: UnlockArgs) -> CliResult {
    use gait_ops::permission::{unlock as op_unlock, GrantScope, UnlockParams};

    let scope = match args.scope.as_str() {
        "global" => GrantScope::Global,
        "agent" => GrantScope::Agent,
        "session" => GrantScope::Session,
        _ => GrantScope::Once,
    };

    let params = UnlockParams {
        gait_dir: args.gait_dir.clone(),
        session_id: args.session_id.clone(),
        scope,
        agent: args.agent.clone(),
    };

    match op_unlock(params) {
        Ok(out) => {
            println!(
                "{}",
                gait_ops::render_json(
                    &serde_json::json!({
                        "session_id": args.session_id,
                        "scope": args.scope,
                        "unlocked_count": out.reply_items.len(),
                        "persisted_grants": out.persisted_grants,
                        "warnings": out.warnings,
                    }),
                    args.pretty
                )
            );
            Ok(())
        }
        Err(e) if matches!(gait_ops::OpError::find(&e), Some(gait_ops::OpError::NotFound(_))) => {
            Err(CliError::not_found(format!(
                "[gait] no pending unlock for session {}",
                args.session_id
            )))
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct ReplyArgs {
    pub session_id: String,
    pub message: String,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

/// `gait load <kind> <name> --session <id>` — append a Toggle(enabled=true)
/// event to a session's log outside a running turn.
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct LoadArgs {
    /// Kind: mcp | skill | plugin
    pub kind: String,
    /// Extension name
    pub name: String,
    /// Session id
    pub session_id: String,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

/// `gait unload <kind> <name> --session <id>` — append a Toggle(enabled=false)
/// event to a session's log outside a running turn.
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct UnloadArgs {
    /// Kind: mcp | skill | plugin
    pub kind: String,
    /// Extension name
    pub name: String,
    /// Session id
    pub session_id: String,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

fn parse_ext_kind(kind: &str) -> Result<gait_ops::extension::ExtKind, String> {
    match kind {
        "mcp" => Ok(gait_ops::extension::ExtKind::Mcp),
        "skill" => Ok(gait_ops::extension::ExtKind::Skill),
        "plugin" => Ok(gait_ops::extension::ExtKind::Plugin),
        other => Err(format!(
            "unknown kind '{}': use mcp, skill, or plugin",
            other
        )),
    }
}

pub fn cmd_load(args: LoadArgs) -> CliResult {
    use gait_ops::extension::{load as op_load, LoadParams};

    let kind = match parse_ext_kind(&args.kind) {
        Ok(k) => k,
        Err(e) => return Err(CliError::invalid_input(format!("[gait] error: {}", e))),
    };
    match op_load(LoadParams {
        gait_dir: args.gait_dir,
        session_id: args.session_id.clone(),
        kind,
        name: args.name.clone(),
    }) {
        Ok(_) => {
            println!(
                "{}",
                gait_ops::render_json(
                    &serde_json::json!({
                        "session_id": args.session_id,
                        "kind": args.kind,
                        "name": args.name,
                        "action": "loaded",
                    }),
                    args.pretty
                )
            );
            Ok(())
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

pub fn cmd_unload(args: UnloadArgs) -> CliResult {
    use gait_ops::extension::{unload as op_unload, LoadParams};

    let kind = match parse_ext_kind(&args.kind) {
        Ok(k) => k,
        Err(e) => return Err(CliError::invalid_input(format!("[gait] error: {}", e))),
    };
    match op_unload(LoadParams {
        gait_dir: args.gait_dir,
        session_id: args.session_id.clone(),
        kind,
        name: args.name.clone(),
    }) {
        Ok(_) => {
            println!(
                "{}",
                gait_ops::render_json(
                    &serde_json::json!({
                        "session_id": args.session_id,
                        "kind": args.kind,
                        "name": args.name,
                        "action": "unloaded",
                    }),
                    args.pretty
                )
            );
            Ok(())
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

pub fn cmd_reply(args: ReplyArgs) -> CliResult {
    match gait_ops::interaction::reply(&args.gait_dir, &args.session_id, args.message) {
        Ok(out) => {
            let kind = match out.kind {
                gait_ops::interaction::ReplyKind::Clarify => "clarify",
                gait_ops::interaction::ReplyKind::Inform => "inform",
            };
            println!(
                "{}",
                gait_ops::render_json(
                    &serde_json::json!({
                        "session_id": args.session_id,
                        "kind": kind,
                        "id": out.id,
                    }),
                    args.pretty
                )
            );
            Ok(())
        }
        Err(e) => Err(CliError::failed_with("[gait] error", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::error::CliError;

    /// The typed variant a command failed with. Panics on an uncoded error,
    /// which `main` would otherwise report as exit 70 with no class.
    fn cli_error(err: &anyhow::Error) -> &CliError {
        coded_error::downcast_ref::<CliError>(err).expect("error is a CliError")
    }

    fn empty_gait_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn persist_without_session_or_all_is_usage() {
        let dir = empty_gait_dir();
        let err = cmd_persist(PersistArgs {
            session_id: None,
            all: false,
            yes: false,
            gait_dir: dir.path().to_path_buf(),
            pretty: false,
        })
        .unwrap_err();
        assert!(matches!(cli_error(&err), CliError::Usage(_)));
        assert_eq!(
            format!("{err:#}"),
            "[gait] error: specify a session id or --all"
        );
    }

    #[test]
    fn discard_without_session_or_all_is_usage() {
        let dir = empty_gait_dir();
        let err = cmd_discard(DiscardArgs {
            session_id: None,
            all: false,
            yes: true,
            force: false,
            gait_dir: dir.path().to_path_buf(),
            pretty: false,
        })
        .unwrap_err();
        assert!(matches!(cli_error(&err), CliError::Usage(_)));
        assert_eq!(
            format!("{err:#}"),
            "[gait] error: specify a session id or --all"
        );
    }

    #[test]
    fn allow_with_retired_agent_scope_is_invalid_input() {
        let dir = empty_gait_dir();
        let err = cmd_allow(AllowArgs {
            session_id: "s1".into(),
            pattern: None,
            scope: Some("agent".into()),
            gait_dir: dir.path().to_path_buf(),
            pretty: false,
        })
        .unwrap_err();
        assert!(matches!(cli_error(&err), CliError::InvalidInput(_)));
        assert_eq!(
            format!("{err:#}"),
            "[gait] error: --scope agent is no longer supported for allow; use --scope mode"
        );
    }

    #[test]
    fn allow_with_unknown_scope_is_invalid_input() {
        let dir = empty_gait_dir();
        let err = cmd_allow(AllowArgs {
            session_id: "s1".into(),
            pattern: None,
            scope: Some("everywhere".into()),
            gait_dir: dir.path().to_path_buf(),
            pretty: false,
        })
        .unwrap_err();
        assert!(matches!(cli_error(&err), CliError::InvalidInput(_)));
    }

    #[test]
    fn load_with_unknown_kind_is_invalid_input() {
        let dir = empty_gait_dir();
        let err = cmd_load(LoadArgs {
            kind: "widget".into(),
            name: "x".into(),
            session_id: "s1".into(),
            gait_dir: dir.path().to_path_buf(),
            pretty: false,
        })
        .unwrap_err();
        assert!(matches!(cli_error(&err), CliError::InvalidInput(_)));
        assert_eq!(
            format!("{err:#}"),
            "[gait] error: unknown kind 'widget': use mcp, skill, or plugin"
        );
    }

    #[test]
    fn unload_with_unknown_kind_is_invalid_input() {
        let dir = empty_gait_dir();
        let err = cmd_unload(UnloadArgs {
            kind: "widget".into(),
            name: "x".into(),
            session_id: "s1".into(),
            gait_dir: dir.path().to_path_buf(),
            pretty: false,
        })
        .unwrap_err();
        assert!(matches!(cli_error(&err), CliError::InvalidInput(_)));
    }

    #[test]
    fn stop_is_failed_until_implemented() {
        let dir = empty_gait_dir();
        let err = cmd_stop(StopArgs {
            session_id: "s1".into(),
            gait_dir: dir.path().to_path_buf(),
            pretty: false,
        })
        .unwrap_err();
        assert!(matches!(cli_error(&err), CliError::Failed(_)));
        assert_eq!(format!("{err:#}"), "[gait] error: stop not yet implemented");
    }
}
