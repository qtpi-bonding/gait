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
    #[arg(long)]
    pub json: bool,
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
    pub json: bool,
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct StopArgs {
    pub session_id: String,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub json: bool,
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
    let yes = args.yes || args.json;
    if args.all {
        run_bulk(
            gait_ops::session::BulkVerb::Persist,
            &args.gait_dir,
            yes,
            args.json,
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
            if !args.json {
                eprintln!("[gait] session {} merged into main", id);
            }
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
    let yes = args.yes || args.json;
    if args.all {
        run_bulk(
            gait_ops::session::BulkVerb::Discard,
            &args.gait_dir,
            yes,
            args.json,
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
        yes,
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
    if args.json {
        println!(
            "{}",
            serde_json::json!({"session_id": id, "action": "discarded"})
        );
    } else {
        eprintln!("[gait] session {} discarded", id);
    }
}

// -----------------------------------------------------------------------------
// Bulk persist/discard with the 3-state consent flow
// -----------------------------------------------------------------------------

/// Bulk persist/discard over every idle, not-running session.
fn run_bulk(verb: gait_ops::session::BulkVerb, gait_dir: &std::path::Path, yes: bool, json: bool) {
    let plan = match gait_ops::session::classify_bulk(gait_dir, verb) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };

    if !json {
        println!("eligible ({}):", plan.eligible.len());
        for id in &plan.eligible {
            println!("  {}\tidle", id);
        }
        if !plan.skipped.is_empty() {
            println!("skipped ({}):", plan.skipped.len());
            for (id, reason) in &plan.skipped {
                println!("  {}\t{}", id, reason);
            }
        }
    }

    if plan.eligible.is_empty() {
        if json {
            println!("{}", plan.to_json());
        } else {
            let label = match verb {
                gait_ops::session::BulkVerb::Persist => "persist",
                gait_ops::session::BulkVerb::Discard => "discard",
            };
            println!("nothing to {}.", label);
        }
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

    if json {
        println!("{}", outcome.to_json());
    } else {
        let past = match verb {
            gait_ops::session::BulkVerb::Persist => "persisted",
            gait_ops::session::BulkVerb::Discard => "discarded",
        };
        print!(
            "{} {}, skipped {}",
            past,
            outcome.acted.len(),
            outcome.skipped.len()
        );
        if !outcome.failed.is_empty() {
            print!(", failed {}", outcome.failed.len());
        }
        println!();
        for (id, e) in &outcome.failed {
            eprintln!("  failed {}: {}", id, e);
        }
        for (id, w) in &outcome.warnings {
            eprintln!("[gait] warning: session {}: {}", id, w);
        }
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
    pub json: bool,
}

pub fn cmd_undo(args: UndoArgs) {
    use gait_ops::session::{undo as op_undo, UndoParams};
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
    if args.list {
        for t in &outcome.turns {
            let mark = t.checkpoint_short.as_deref().unwrap_or("--------");
            println!("turn {}\t[{}]\t{}", t.turn, mark, t.summary);
        }
        return;
    }
    if let Some(sha) = &outcome.checkpoint_short {
        if outcome.code_undone {
            println!("code restored to turn ({})", &sha[..sha.len().min(8)]);
        } else {
            println!("turn has no code checkpoint (non-git codebase); code not undone");
        }
    }
    if outcome.transcript_undone {
        println!("transcript undone to turn");
    }
}

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct SessionsArgs {
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub json: bool,
}

pub fn cmd_sessions(args: SessionsArgs) {
    match gait_ops::session::list(&args.gait_dir) {
        Ok(out) if out.active.is_empty() => println!("No active sessions."),
        Ok(out) => {
            for id in out.active {
                println!("{}", id);
            }
        }
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
    pub json: bool,
}

pub fn cmd_ps(args: PsArgs) {
    match gait_ops::session::ps(&args.gait_dir) {
        Ok(out) => {
            if args.json {
                println!("{}", out.to_json());
            } else {
                for (id, state) in out.to_rows() {
                    println!("{}\t{}", id, state);
                }
            }
        }
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
    pub json: bool,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
}

pub fn cmd_stats(args: StatsArgs) {
    match gait_ops::session::stats(&args.gait_dir, &args.session_id) {
        Ok(u) => {
            if args.json {
                println!("{}", u.to_json());
            } else {
                println!("session {}", args.session_id);
                println!("  input        {}", u.input);
                println!("  output       {}", u.output);
                println!("  cache read   {}", u.cache_read);
                println!("  cache write  {}", u.cache_write);
                println!("  total tokens {}", u.total);
            }
        }
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
    pub json: bool,
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
            eprintln!(
                "[gait] approved {} (scope: {:?})",
                args.session_id, out.scope
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
    pub json: bool,
}

pub fn cmd_deny(args: DenyArgs) {
    use gait_ops::permission::deny as op_deny;

    match op_deny(&args.gait_dir, &args.session_id) {
        Ok(out) if out.reply_items.is_empty() => {
            eprintln!(
                "[gait] no pending permissions for session {}",
                args.session_id
            );
        }
        Ok(out) => {
            eprintln!(
                "[gait] denied {} pending permission(s) for session {}",
                out.reply_items.len(),
                args.session_id
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
    pub json: bool,
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
            for w in &out.warnings {
                eprintln!("[gait] warning: {w}");
            }
            if out.reply_items.is_empty() {
                eprintln!(
                    "[gait] no pending unlock items for session {}",
                    args.session_id
                );
                return;
            }
            eprintln!(
                "[gait] unlocked {} item(s) for session {} (scope: {})",
                out.reply_items.len(),
                args.session_id,
                args.scope,
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
    pub json: bool,
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
            "Loaded {} '{}' into session '{}'.",
            args.kind, args.name, args.session_id
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
            "Unloaded {} '{}' from session '{}'.",
            args.kind, args.name, args.session_id
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
            eprintln!(
                "[gait] answered pending {} '{}' for session {}",
                kind, out.id, args.session_id
            );
        }
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    }
}
