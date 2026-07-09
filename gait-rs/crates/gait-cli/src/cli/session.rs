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

fn print_bulk_json(
    eligible: &[String],
    skipped: &[(String, String)],
    acted: &[String],
    failed: &[(String, String)],
) {
    let v = serde_json::json!({
        "eligible": eligible,
        "skipped": skipped.iter()
            .map(|(id, s)| serde_json::json!({"session_id": id, "state": s}))
            .collect::<Vec<_>>(),
        "acted": acted,
        "failed": failed.iter()
            .map(|(id, e)| serde_json::json!({"session_id": id, "error": e}))
            .collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

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
            print_bulk_json(&plan.eligible, &plan.skipped, &[], &[]);
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
        print_bulk_json(
            &outcome.eligible,
            &outcome.skipped,
            &outcome.acted,
            &outcome.failed,
        );
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
                let arr: Vec<serde_json::Value> = out
                    .rows
                    .iter()
                    .map(|r| serde_json::json!({"session_id": r.session_id, "state": r.state}))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&arr).unwrap_or_else(|e| {
                        eprintln!("[gait] error serializing ps: {}", e);
                        std::process::exit(1);
                    })
                );
            } else {
                for r in &out.rows {
                    println!("{}\t{}", r.session_id, r.state);
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
                println!(
                    "{{\"input\":{},\"output\":{},\"cache_read\":{},\"cache_write\":{}}}",
                    u.input, u.output, u.cache_read, u.cache_write
                );
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
    use gait_core::agent_frontmatter::{resolve_markdown_context_path, MarkdownKind};
    use gait_core::permissions::{compute_allow_decisions, AllowParams, AllowScope};
    use gait_core::session::log::{session_state, write_permissions_reply, SessionState};

    let scope = match args.scope.as_deref() {
        Some("global") => AllowScope::Global,
        Some("agent") => AllowScope::Agent,
        // "session" scope not yet in AllowScope; treat as Once until gait-core adds it
        Some("session") | Some("once") | None => AllowScope::Once,
        Some(other) => {
            eprintln!(
                "[gait] error: unknown --scope '{}' (expected global|agent|session|once)",
                other
            );
            std::process::exit(1);
        }
    };

    let agent_path = args
        .agent
        .as_deref()
        .map(|n| resolve_markdown_context_path(&args.gait_dir, MarkdownKind::Agent, n));

    let params = AllowParams {
        scope,
        pattern: args.pattern,
    };

    let tx_path = gait_core::session::live::log_path(&args.gait_dir, &args.session_id);
    let state = match session_state(&tx_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };
    let items = match state {
        SessionState::AwaitingPermission { items } => items,
        _ => {
            eprintln!(
                "[gait] no pending permissions for session {}",
                args.session_id
            );
            std::process::exit(1);
        }
    };
    let reply_items = match compute_allow_decisions(
        &items,
        &params,
        &args.gait_dir,
        &args.session_id,
        agent_path.as_deref(),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = write_permissions_reply(&tx_path, "user", reply_items) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    eprintln!(
        "[gait] approved {} (scope: {:?})",
        args.session_id, params.scope
    );
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
    use gait_core::proto::gait::log::v1 as pb;
    use gait_core::session::log::{session_state, write_permissions_reply, SessionState};

    let tx_path = gait_core::session::live::log_path(&args.gait_dir, &args.session_id);
    let state = match session_state(&tx_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };
    let items = match state {
        SessionState::AwaitingPermission { items } => items,
        _ => {
            eprintln!(
                "[gait] no pending permissions for session {}",
                args.session_id
            );
            std::process::exit(1);
        }
    };

    if items.is_empty() {
        eprintln!(
            "[gait] no pending permissions for session {}",
            args.session_id
        );
        return;
    }

    let reply: Vec<pb::PermissionsReplyItem> = items
        .iter()
        .map(|it| pb::PermissionsReplyItem {
            id: it.id.clone(),
            decision: pb::Decision::Deny as i32,
            text: None,
            scope: pb::Scope::Unspecified as i32,
        })
        .collect();

    if let Err(e) = write_permissions_reply(&tx_path, "user", reply) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    eprintln!(
        "[gait] denied {} pending permission(s) for session {}",
        items.len(),
        args.session_id
    );
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

/// Route a list of `UnlockItem`s through `SandboxGrant`. Replaces the two
/// previously-duplicated `match item.dimension` blocks in the `Global` and
/// `Agent` arms of `cmd_unlock` — both now share these warning semantics.
fn persist_grant_items(
    items: &[gait_core::proto::gait::log::v1::UnlockItem],
    target_dir: &std::path::Path,
) {
    use gait_core::sandbox_grant::SandboxGrant;
    for item in items {
        match SandboxGrant::from_dimension(&item.dimension) {
            Some(g) => {
                if let Err(e) = g.apply(target_dir, &item.resource) {
                    eprintln!("[gait] warning: could not persist grant to config: {e}");
                }
            }
            None => eprintln!(
                "[gait] warning: unknown dimension {:?}, skipping config write",
                item.dimension
            ),
        }
    }
}

pub fn cmd_unlock(args: UnlockArgs) {
    use gait_core::proto::gait::log::v1 as pb;
    use gait_core::session::log::{session_state, write_unlock_reply, SessionState};

    let scope = match args.scope.as_str() {
        "global" => pb::Scope::Global,
        "agent" => pb::Scope::Agent,
        "session" => pb::Scope::Session,
        _ => pb::Scope::Once,
    };

    let tx_path = gait_core::session::live::log_path(&args.gait_dir, &args.session_id);

    let state = match session_state(&tx_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };

    let items = match state {
        SessionState::AwaitingUnlock { items } => items,
        _ => {
            eprintln!("[gait] no pending unlock for session {}", args.session_id);
            std::process::exit(1);
        }
    };

    if items.is_empty() {
        eprintln!(
            "[gait] no pending unlock items for session {}",
            args.session_id
        );
        return;
    }

    // For global grants, persist to config first
    if matches!(scope, pb::Scope::Global) {
        persist_grant_items(&items, &args.gait_dir);
    } else if matches!(scope, pb::Scope::Agent) {
        // Agent-scoped: write to agent gait_dir sandbox block
        if let Some(agent_name) = &args.agent {
            use gait_core::agent_frontmatter::{resolve_markdown_context_path, MarkdownKind};
            let agent_path =
                resolve_markdown_context_path(&args.gait_dir, MarkdownKind::Agent, agent_name);
            if let Some(parent) = agent_path.parent() {
                persist_grant_items(&items, parent);
            }
        } else {
            eprintln!(
                "[gait] warning: --agent required for agent-scope grants; writing to log only"
            );
        }
    }

    // Always write the UnlockReply to the log
    let reply_items: Vec<pb::UnlockReplyItem> = items
        .iter()
        .map(|it| pb::UnlockReplyItem {
            id: it.id.clone(),
            decision: pb::Decision::Allow as i32,
            scope: scope as i32,
        })
        .collect();

    if let Err(e) = write_unlock_reply(&tx_path, "user", reply_items) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }

    eprintln!(
        "[gait] unlocked {} item(s) for session {} (scope: {})",
        items.len(),
        args.session_id,
        args.scope,
    );
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

/// Resolve `tx_path`/`session_dir` for a session id and validate that the target
/// name is present in the global registry for the given kind. The full
/// agent_whitelist enforcement (Phase 3's `resolve_visible_set`) is *not*
/// reachable from this CLI context — the agent_whitelist is normally threaded
/// through Profile/ResolvedProfile at turn-setup time, not persisted anywhere
/// reloadable from just gait_dir+session_id. That's a known, narrower-than-ideal
/// scope here; the in-turn `enable` enforcement at `turn/execute.rs:370` and
/// the `--scope session` register/unregister writes are the required Phase 3
/// enforcement points per the plan. This is the minimum: a name not present in
/// any registry must error clearly.
fn resolve_load_targets(
    kind: &str,
    name: &str,
    gait_dir: &std::path::Path,
    session_id: &str,
) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    match kind {
        "mcp" | "skill" | "plugin" => {}
        other => {
            return Err(format!(
                "unknown kind '{}': use mcp, skill, or plugin",
                other
            ));
        }
    }

    // `gait_core::mcp`/`gait_core::wasm_plugins` are `pub(crate)` (not
    // reachable from this crate), so mcp/plugin existence is checked the same
    // way admin.rs's own disk-first branches do: by the canonical disk path,
    // not a scan helper. `skills::scan_skills` is public and used directly.
    let in_global_registry: bool = match kind {
        "skill" => gait_core::skills::scan_skills(gait_dir)
            .iter()
            .any(|s| s.name == name),
        "mcp" => gait_dir.join("mcp").join(format!("{}.json", name)).exists(),
        "plugin" => gait_dir.join("plugins").join(name).exists(),
        _ => unreachable!(),
    };
    if !in_global_registry {
        return Err(format!(
            "{} '{}' is not registered; run `gait register {} {}` first",
            kind, name, kind, name
        ));
    }

    let tx_path = gait_core::session::live::log_path(gait_dir, session_id);
    let session_dir = gait_core::session::live::worktree_dir(gait_dir, session_id);
    Ok((tx_path, session_dir))
}

pub fn cmd_load(args: LoadArgs) {
    let (tx_path, session_dir) =
        match resolve_load_targets(&args.kind, &args.name, &args.gait_dir, &args.session_id) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        };
    if let Err(e) =
        gait_core::session::log::write_toggle(&tx_path, &session_dir, &args.kind, &args.name, true)
    {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    println!(
        "Loaded {} '{}' into session '{}'.",
        args.kind, args.name, args.session_id
    );
}

pub fn cmd_unload(args: UnloadArgs) {
    let (tx_path, session_dir) =
        match resolve_load_targets(&args.kind, &args.name, &args.gait_dir, &args.session_id) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        };
    if let Err(e) =
        gait_core::session::log::write_toggle(&tx_path, &session_dir, &args.kind, &args.name, false)
    {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    println!(
        "Unloaded {} '{}' from session '{}'.",
        args.kind, args.name, args.session_id
    );
}
pub fn cmd_reply(_args: ReplyArgs) {
    eprintln!("[gait] error: reply not yet implemented");
    std::process::exit(1);
}

// --- Phase 4: load/unload tests ---
#[cfg(test)]
mod load_unload_tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a minimal gait-dir with a single registered skill at
    /// `skills/tdd/SKILL.md` and a quiescent session's log directory. No git
    /// worktree needed — `write_toggle` only needs `tx_path`'s parent dir to
    /// exist and `session_dir` to have no live lock file.
    fn ws_with_skill_and_session() -> (TempDir, String) {
        let ws = TempDir::new().unwrap();
        let session_id = "load0001".to_string();
        std::fs::create_dir_all(ws.path().join("skills/tdd")).unwrap();
        std::fs::write(
            ws.path().join("skills/tdd/SKILL.md"),
            "---\nname: tdd\ndescription: x\nenabled: false\n---\n# body\n",
        )
        .unwrap();
        let tx_path = gait_core::session::live::log_path(ws.path(), &session_id);
        std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();
        (ws, session_id)
    }

    fn read_toggle(tx_path: &std::path::Path) -> gait_core::proto::gait::log::v1::Toggle {
        use gait_core::proto::gait::log::v1::{event, payload};
        let events = gait_core::session::log::read_events(tx_path).unwrap();
        match &events[0].body {
            Some(event::Body::Payload(p)) => match &p.kind {
                Some(payload::Kind::Toggle(t)) => t.clone(),
                other => panic!("expected Toggle payload, got {other:?}"),
            },
            other => panic!("expected payload body, got {other:?}"),
        }
    }

    #[test]
    fn cmd_load_appends_toggle_enabled_true_to_quiescent_log() {
        let (ws, sid) = ws_with_skill_and_session();
        let tx_path = gait_core::session::live::log_path(ws.path(), &sid);
        cmd_load(LoadArgs {
            kind: "skill".into(),
            name: "tdd".into(),
            session_id: sid.clone(),
            gait_dir: ws.path().to_path_buf(),
        });
        let toggle = read_toggle(&tx_path);
        assert_eq!(toggle.name, "tdd");
        assert!(toggle.enabled, "load must set enabled=true");
    }

    #[test]
    fn cmd_unload_appends_toggle_enabled_false_to_quiescent_log() {
        let (ws, sid) = ws_with_skill_and_session();
        let tx_path = gait_core::session::live::log_path(ws.path(), &sid);
        cmd_unload(UnloadArgs {
            kind: "skill".into(),
            name: "tdd".into(),
            session_id: sid.clone(),
            gait_dir: ws.path().to_path_buf(),
        });
        let toggle = read_toggle(&tx_path);
        assert_eq!(toggle.name, "tdd");
        assert!(!toggle.enabled, "unload must set enabled=false");
    }

    #[test]
    fn resolve_load_targets_rejects_unknown_name() {
        let ws = TempDir::new().unwrap();
        let err = resolve_load_targets("skill", "no-such-skill", ws.path(), "anyid").unwrap_err();
        assert!(
            err.contains("not registered"),
            "expected 'not registered' error, got: {err}"
        );
    }
}
