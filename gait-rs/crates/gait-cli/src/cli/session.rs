//! Session lifecycle + inspection commands.
//!
//! All logic lives in `gait_ops::session`. The functions here are thin
//! adapters: map clap `Args` → op params → call op → render outcome/error
//! → `exit(1)` on error. The CLI keeps the 3-state consent flow
//! (`confirm_action -> Confirmed::{Yes, No, RequiresYes}`) for the bulk
//! persist/discard prompts; the op itself takes a `confirmed: bool` and
//! refuses (`OpError::ConfirmationRequired`) on `false`.
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

pub fn cmd_persist(args: PersistArgs) {
    // `--json` used to double as an implicit `--yes` for scripted callers.
    // Output is unconditionally JSON now, so that shortcut is gone — a
    // non-interactive caller must pass `--yes` explicitly (or rely on
    // `confirm_action`'s existing non-TTY `RequiresYes` gate).
    if args.all {
        run_bulk(
            gait_ops::session::BulkVerb::Persist,
            &args.gait_dir,
            args.yes,
            args.pretty,
        );
        return;
    }
    let Some(id) = args.session_id.as_deref() else {
        eprintln!("[gait] error: specify a session id or --all");
        std::process::exit(1);
    };
    // One-element plan: render it, then call act(plan, confirmed).
    let plan = match gait_ops::session::classify_bulk(
        &args.gait_dir,
        gait_ops::session::BulkVerb::Persist,
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };
    if !plan.eligible.iter().any(|s| s == id) {
        eprintln!("[gait] error: session {} is not eligible to persist", id);
        std::process::exit(1);
    }
    let one: Vec<String> = vec![id.to_string()];
    let plan = gait_ops::session::BulkPlan {
        verb: gait_ops::session::BulkVerb::Persist,
        eligible: one,
        skipped: plan.skipped,
    };
    match gait_ops::session::bulk_act(&args.gait_dir, &plan, true) {
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
        }
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn cmd_discard(args: DiscardArgs) {
    if args.all {
        run_bulk(
            gait_ops::session::BulkVerb::Discard,
            &args.gait_dir,
            args.yes,
            args.pretty,
        );
        return;
    }
    let Some(id) = args.session_id.as_deref() else {
        eprintln!("[gait] error: specify a session id or --all");
        std::process::exit(1);
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
            return;
        }
        Confirmed::RequiresYes => {
            eprintln!("[gait] error: refusing to discard non-interactively without --yes");
            std::process::exit(1);
        }
    }
    let plan = gait_ops::session::BulkPlan {
        verb: gait_ops::session::BulkVerb::Discard,
        eligible: vec![id.to_string()],
        skipped: Vec::new(),
    };
    if let Err(e) = gait_ops::session::bulk_act(&args.gait_dir, &plan, true) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    println!(
        "{}",
        gait_ops::render_json(
            &serde_json::json!({"session_id": id, "action": "discarded"}),
            args.pretty
        )
    );
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
    pretty: bool,
) {
    let plan = match gait_ops::session::classify_bulk(gait_dir, verb) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };

    // The preview is itself a real result (what will happen) — always JSON.
    println!("{}", plan.to_json(pretty));

    if plan.eligible.is_empty() {
        return;
    }

    // 3-state consent: Yes / No / RequiresYes. `No` aborts quietly (exit 0),
    // `RequiresYes` (non-interactive without --yes) errors and exits 1.
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
            return;
        }
        Confirmed::RequiresYes => {
            eprintln!(
                "[gait] error: refusing to {} --all non-interactively without --yes",
                label
            );
            std::process::exit(1);
        }
    }

    let outcome = match gait_ops::session::bulk_act(gait_dir, &plan, true) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };

    println!("{}", outcome.to_json(pretty));
    for (id, e) in &outcome.failed {
        eprintln!("  failed {}: {}", id, e);
    }
    for (id, w) in &outcome.warnings {
        eprintln!("[gait] warning: session {}: {}", id, w);
    }
}

pub fn cmd_stop(_args: StopArgs) {
    eprintln!("[gait] error: stop not yet implemented");
    std::process::exit(1);
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

pub fn cmd_undo(args: UndoArgs) {
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
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };
    println!("{}", outcome.to_json(pretty));
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

pub fn cmd_sessions(args: SessionsArgs) {
    match gait_ops::session::list(&args.gait_dir, args.last) {
        Ok(out) => println!("{}", out.to_json(args.pretty)),
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
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

pub fn cmd_ps(args: PsArgs) {
    match gait_ops::session::ps(&args.gait_dir) {
        Ok(out) => println!("{}", out.to_json(args.pretty)),
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
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

pub fn cmd_stats(args: StatsArgs) {
    match gait_ops::session::stats(&args.gait_dir, &args.session_id) {
        Ok(u) => println!("{}", u.to_json(args.pretty)),
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
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
pub fn cmd_log(args: LogArgs) {
    match gait_ops::session::summary(&args.gait_dir, &args.session_id) {
        Ok(out) => println!("{}", out.to_json(args.pretty)),
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
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
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

pub fn cmd_allow(args: AllowArgs) {
    use gait_ops::permission::{allow as op_allow, AllowParams, GrantScope};

    let scope = match args.scope.as_deref() {
        Some("global") => GrantScope::Global,
        Some("agent") => GrantScope::Agent,
        Some("session") => GrantScope::Session,
        Some("once") | None => GrantScope::Once,
        Some(other) => {
            eprintln!(
                "[gait] error: unknown --scope '{}' (expected global|agent|session|once)",
                other
            );
            std::process::exit(1);
        }
    };

    let params = AllowParams {
        scope,
        pattern: args.pattern,
        agent: args.agent,
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
        }
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
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

pub fn cmd_deny(args: DenyArgs) {
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
        }
        Err(gait_ops::OpError::NotFound(_)) => {
            eprintln!(
                "[gait] no pending permissions for session {}",
                args.session_id
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
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

pub fn cmd_unlock(args: UnlockArgs) {
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
        }
        Err(gait_ops::OpError::NotFound(_)) => {
            eprintln!("[gait] no pending unlock for session {}", args.session_id);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
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

pub fn cmd_load(args: LoadArgs) {
    use gait_ops::extension::{load as op_load, LoadParams};

    let kind = match parse_ext_kind(&args.kind) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };
    match op_load(LoadParams {
        gait_dir: args.gait_dir,
        session_id: args.session_id.clone(),
        kind,
        name: args.name.clone(),
    }) {
        Ok(_) => println!(
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
        ),
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn cmd_unload(args: UnloadArgs) {
    use gait_ops::extension::{unload as op_unload, LoadParams};

    let kind = match parse_ext_kind(&args.kind) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };
    match op_unload(LoadParams {
        gait_dir: args.gait_dir,
        session_id: args.session_id.clone(),
        kind,
        name: args.name.clone(),
    }) {
        Ok(_) => println!(
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
        ),
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    }
}
pub fn cmd_reply(args: ReplyArgs) {
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
        }
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    }
}
