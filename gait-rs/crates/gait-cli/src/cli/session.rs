//! Session lifecycle + inspection commands.
//!
//! Public clap structs: `PersistArgs`, `DiscardArgs`, `StopArgs`, `SessionsArgs`,
//! `PsArgs`, `StatsArgs`, `AllowArgs`, `DenyArgs`, `UnlockArgs`, `UndoArgs`,
//! `ReplyArgs`.

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

/// Discard a single session: lock it, confirm, tear it down (no merge).
fn discard_one(gait_dir: &std::path::Path, id: &str, yes: bool, json: bool) -> anyhow::Result<()> {
    let session_dir = gait_dir.join("sessions").join(id);
    let _lock = gait_core::turn::SessionLock::acquire(&session_dir)
        .map_err(|e| anyhow::anyhow!("cannot discard: {} (is a turn in progress?)", e))?;
    match confirm_action(
        &format!("discard session {id} permanently (no merge, unrecoverable)"),
        yes,
    ) {
        Confirmed::Yes => {}
        Confirmed::No => {
            eprintln!("[gait] aborted");
            return Ok(());
        }
        Confirmed::RequiresYes => {
            anyhow::bail!("refusing to discard non-interactively without --yes")
        }
    }
    gait_core::git::Repos::production()
        .traces
        .discard_session(gait_dir, id)?;
    if json {
        println!(
            "{}",
            serde_json::json!({"session_id": id, "action": "discarded"})
        );
    } else {
        eprintln!("[gait] session {id} discarded");
    }
    Ok(())
}

pub fn cmd_persist(args: PersistArgs) {
    let yes = args.yes || args.json;
    if args.all {
        run_bulk(Verb::Persist, &args.gait_dir, yes, args.json);
        return;
    }
    let Some(id) = args.session_id.as_deref() else {
        eprintln!("[gait] error: specify a session id or --all");
        std::process::exit(1);
    };
    let repos = gait_core::git::Repos::production();
    if let Err(e) = repos.traces.finish_session(&args.gait_dir, id) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    if let Err(e) = repos.checkpoints.remove(&args.gait_dir, id) {
        eprintln!("warning: failed to remove checkpoint db for {}: {}", id, e);
    }
    eprintln!("[gait] session {} merged into main", id);
}

pub fn cmd_discard(args: DiscardArgs) {
    let yes = args.yes || args.json;
    if args.all {
        run_bulk(Verb::Discard, &args.gait_dir, yes, args.json);
        return;
    }
    let Some(id) = args.session_id.as_deref() else {
        eprintln!("[gait] error: specify a session id or --all");
        std::process::exit(1);
    };
    if let Err(e) = discard_one(&args.gait_dir, id, yes, args.json) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
}

#[derive(Clone, Copy)]
enum Verb {
    Persist,
    Discard,
}

impl Verb {
    fn label(&self) -> &'static str {
        match self {
            Verb::Persist => "persist",
            Verb::Discard => "discard",
        }
    }
    fn past(&self) -> &'static str {
        match self {
            Verb::Persist => "persisted",
            Verb::Discard => "discarded",
        }
    }
    fn act(&self, gait_dir: &std::path::Path, id: &str) -> anyhow::Result<()> {
        let repos = gait_core::git::Repos::production();
        match self {
            Verb::Persist => {
                repos.traces.finish_session(gait_dir, id)?;
                if let Err(e) = repos.checkpoints.remove(gait_dir, id) {
                    eprintln!("warning: failed to remove checkpoint db for {}: {}", id, e);
                }
                Ok(())
            }
            Verb::Discard => repos.traces.discard_session(gait_dir, id),
        }
    }
}

/// Walk `sessions/*`; classify each dir as eligible (idle + not running) or
/// skipped-with-reason. Dirs whose log.jsonl won't parse are silently omitted
/// (non-session dirs), matching `cmd_ps`. Results are sorted by id.
fn classify_bulk_targets(
    gait_dir: &std::path::Path,
) -> anyhow::Result<(Vec<String>, Vec<(String, String)>)> {
    use gait_core::session::log::{session_state, SessionState};

    let sessions_dir = gait_dir.join("sessions");
    let mut eligible: Vec<String> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();

    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(_) => return Ok((eligible, skipped)), // no sessions dir => nothing
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };
        // A dir with no log.jsonl is not a started session. `read_events` treats a
        // MISSING file as empty => `session_state` returns Idle, which would wrongly
        // mark such a dir eligible — so skip it explicitly here. (A malformed log
        // still falls through to the `Err(_) => continue` arm below.)
        if !path.join("log.jsonl").exists() {
            continue;
        }
        let reason: Option<String> = match session_state(&path.join("log.jsonl")) {
            Ok(SessionState::Idle) => {
                if gait_core::turn::SessionLock::is_running(&path) {
                    Some("running".to_string())
                } else {
                    None
                }
            }
            Ok(SessionState::AwaitingPermission { .. }) => Some("awaiting-permission".to_string()),
            Ok(SessionState::AwaitingClarify { .. }) => Some("awaiting-clarify".to_string()),
            Ok(SessionState::AwaitingInform { .. }) => Some("awaiting-inform".to_string()),
            Ok(SessionState::AwaitingUnlock { .. }) => Some("awaiting-unlock".to_string()),
            Err(_) => continue, // unparseable => not a session dir
        };
        match reason {
            None => eligible.push(id),
            Some(r) => skipped.push((id, r)),
        }
    }
    eligible.sort();
    skipped.sort();
    Ok((eligible, skipped))
}

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
fn run_bulk(verb: Verb, gait_dir: &std::path::Path, yes: bool, json: bool) {
    let (eligible, skipped) = match classify_bulk_targets(gait_dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };

    if !json {
        println!("eligible ({}):", eligible.len());
        for id in &eligible {
            println!("  {}\tidle", id);
        }
        if !skipped.is_empty() {
            println!("skipped ({}):", skipped.len());
            for (id, reason) in &skipped {
                println!("  {}\t{}", id, reason);
            }
        }
    }

    if eligible.is_empty() {
        if json {
            print_bulk_json(&eligible, &skipped, &[], &[]);
        } else {
            println!("nothing to {}.", verb.label());
        }
        return;
    }

    match confirm_action(
        &format!("{} {} session(s)?", verb.label(), eligible.len()),
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
                verb.label()
            );
            std::process::exit(1);
        }
    }

    let mut acted: Vec<String> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for id in &eligible {
        let session_dir = gait_dir.join("sessions").join(id);
        // Re-check the lock at action time; a run may have started since classify.
        let _lock = match gait_core::turn::SessionLock::acquire(&session_dir) {
            Ok(l) => l,
            Err(_) => {
                failed.push((id.clone(), "started running".to_string()));
                continue;
            }
        };
        match verb.act(gait_dir, id) {
            Ok(()) => acted.push(id.clone()),
            Err(e) => failed.push((id.clone(), e.to_string())),
        }
    }

    if json {
        print_bulk_json(&eligible, &skipped, &acted, &failed);
    } else {
        print!("{} {}, skipped {}", verb.past(), acted.len(), skipped.len());
        if !failed.is_empty() {
            print!(", failed {}", failed.len());
        }
        println!();
        for (id, e) in &failed {
            eprintln!("  failed {}: {}", id, e);
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
    if let Err(e) = undo_command(
        &args.session_id,
        args.turn,
        args.code,
        args.transcript,
        args.list,
        args.gait_dir,
    ) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
}

fn undo_command(
    session_id: &str,
    turn: Option<u32>,
    code: bool,
    transcript: bool,
    list: bool,
    gait_dir: PathBuf,
) -> anyhow::Result<()> {
    use gait_core::proto::gait::log::v1::{event, payload};
    use gait_core::session::log::read_events;

    if list && turn.is_some() {
        anyhow::bail!("--list and --turn are mutually exclusive");
    }

    let session_dir = gait_dir.join("sessions").join(session_id);
    let tx_path = session_dir.join("log.jsonl");
    let events = read_events(&tx_path)?;

    // Collect (turn_index, summary, checkpoint) from TurnComplete events.
    let mut turns: Vec<(u32, String, Option<String>)> = Vec::new();
    for e in &events {
        if let Some(event::Body::Payload(p)) = &e.body {
            if let Some(payload::Kind::TurnComplete(tc)) = &p.kind {
                let idx = turns.len() as u32 + 1;
                let message = tc
                    .output
                    .as_ref()
                    .map(|o| o.message.clone())
                    .unwrap_or_default();
                turns.push((idx, message, tc.checkpoint.clone()));
            }
        }
    }

    if list {
        for (idx, summary, cp) in &turns {
            let mark = cp
                .as_deref()
                .map(|s| &s[..s.len().min(8)])
                .unwrap_or("--------");
            println!("turn {idx}  [{mark}]  {summary}");
        }
        return Ok(());
    }

    let turn = turn.ok_or_else(|| anyhow::anyhow!("--turn <n> is required (or use --list)"))?;
    let (_, _, checkpoint) = turns
        .iter()
        .find(|(idx, _, _)| *idx == turn)
        .ok_or_else(|| anyhow::anyhow!("no turn {} in session {}", turn, session_id))?
        .clone();

    // Default (neither flag) = both axes.
    let do_code = code || !transcript;
    let do_transcript = transcript || !code;

    let _lock = gait_core::turn::SessionLock::acquire(&session_dir)
        .map_err(|e| anyhow::anyhow!("cannot undo: {} (is a turn in progress?)", e))?;

    if do_code {
        let codebase = gait_core::session::read_session_meta(&gait_dir, session_id)?
            .codebase_path
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("session has no codebase_path; cannot undo code"))?;
        let repos = gait_core::git::Repos::production();
        match checkpoint.as_deref() {
            Some(sha) => {
                repos
                    .checkpoints
                    .restore_to(&gait_dir, session_id, &codebase, sha)?;
                println!(
                    "code restored to turn {turn} ({})",
                    &sha[..sha.len().min(8)]
                );
            }
            None => {
                println!("turn {turn} has no code checkpoint (non-git codebase); code not undone")
            }
        }
    }

    if do_transcript {
        undo_transcript_to_turn(&gait_dir, session_id, turn)?;
        println!("transcript undone to turn {turn}");
    }
    Ok(())
}

fn undo_transcript_to_turn(
    gait_dir: &std::path::Path,
    session_id: &str,
    turn: u32,
) -> anyhow::Result<()> {
    let session_dir = gait_dir.join("sessions").join(session_id);
    let repo = git2::Repository::open(&session_dir)?;
    let needle = format!("session({}): turn {}", session_id, turn);
    let mut walk = repo.revwalk()?;
    walk.push_head()?;
    let mut target: Option<git2::Oid> = None;
    for oid in walk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        if commit.summary() == Some(needle.as_str()) {
            target = Some(oid);
            break;
        }
    }
    let oid = target.ok_or_else(|| anyhow::anyhow!(
        "no commit for turn {} in session {} (transcript undo applies to interactive turn sessions, not workflows)",
        turn, session_id))?;
    let obj = repo.find_object(oid, None)?;
    repo.reset(&obj, git2::ResetType::Hard, None)?;
    Ok(())
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
    match gait_core::git::Repos::production()
        .traces
        .list_active_sessions(&args.gait_dir)
    {
        Ok(sessions) if sessions.is_empty() => println!("No active sessions."),
        Ok(sessions) => {
            for id in sessions {
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
    use gait_core::session::log::{session_state, SessionState};

    let sessions_dir = args.gait_dir.join("sessions");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };

    let mut rows: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let tx_path = path.join("log.jsonl");
        let state_str = match session_state(&tx_path) {
            Ok(SessionState::Idle) => "idle".to_string(),
            Ok(SessionState::AwaitingPermission { .. }) => "awaiting-permission".to_string(),
            Ok(SessionState::AwaitingClarify { .. }) => "awaiting-clarify".to_string(),
            Ok(SessionState::AwaitingInform { .. }) => "awaiting-inform".to_string(),
            Ok(SessionState::AwaitingUnlock { .. }) => "awaiting-unlock".to_string(),
            Err(_) => continue, // skip unparseable / non-session dirs
        };
        rows.push((id, state_str));
    }

    if args.json {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|(id, st)| serde_json::json!({"session_id": id, "state": st}))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&arr).unwrap_or_else(|e| {
                eprintln!("[gait] error serializing ps: {}", e);
                std::process::exit(1);
            })
        );
        return;
    }

    for (id, st) in &rows {
        println!("{}\t{}", id, st);
    }
}

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

    let tx_path = args
        .gait_dir
        .join("sessions")
        .join(&args.session_id)
        .join("log.jsonl");
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

    let tx_path = args
        .gait_dir
        .join("sessions")
        .join(&args.session_id)
        .join("log.jsonl");
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

    let tx_path = args
        .gait_dir
        .join("sessions")
        .join(&args.session_id)
        .join("log.jsonl");

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
pub struct StatsArgs {
    pub session_id: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: PathBuf,
}

pub fn cmd_stats(args: StatsArgs) {
    use gait_core::proto::wire::read_events;
    use gait_core::session::retrace::total_usage;

    let tx_path = args
        .gait_dir
        .join("sessions")
        .join(&args.session_id)
        .join("log.jsonl");
    let events = read_events(&tx_path).unwrap_or_else(|e| {
        eprintln!("[gait] cannot read session {}: {e}", args.session_id);
        std::process::exit(1);
    });
    let u = total_usage(&events);

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
        println!(
            "  total tokens {}",
            u.input + u.output + u.cache_read + u.cache_write
        );
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

pub fn cmd_reply(_args: ReplyArgs) {
    eprintln!("[gait] error: reply not yet implemented");
    std::process::exit(1);
}

#[cfg(test)]
mod bulk_tests {
    use super::*;
    // NB: no `TraceRepo` import — trait-object method calls (`traces.finish_session`
    // etc.) don't need the trait in scope, and `gait_core::git::TraceRepo` is not a
    // public path (the trait lives at `gait_core::git::trace::TraceRepo`).
    use gait_core::git::Repos;
    use tempfile::TempDir;

    // Build a gait-dir with one committed session worktree; returns (tempdir, id).
    fn ws_with_session(id: &str) -> TempDir {
        let ws = TempDir::new().unwrap();
        let repo = git2::Repository::init(ws.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "t").unwrap();
        cfg.set_str("user.email", "t@t.com").unwrap();
        let sig = git2::Signature::now("t", "t@t.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        drop(tree);
        repo.find_branch("master", git2::BranchType::Local)
            .or_else(|_| repo.find_branch("main", git2::BranchType::Local))
            .and_then(|mut b| {
                b.rename("main", false)?;
                Ok(())
            })
            .ok();
        let traces = Repos::production().traces;
        traces.create_session_worktree(ws.path(), id).unwrap();
        ws
    }

    #[test]
    fn confirm_action_requires_yes_when_non_tty_and_no_flag() {
        // Test harness has no TTY on stdin.
        assert!(matches!(
            confirm_action("go?", false),
            Confirmed::RequiresYes
        ));
    }

    #[test]
    fn confirm_action_yes_flag_short_circuits() {
        assert!(matches!(confirm_action("go?", true), Confirmed::Yes));
    }

    #[test]
    fn discard_one_with_yes_removes_session() {
        let ws = ws_with_session("scli001");
        discard_one(ws.path(), "scli001", true, false).unwrap();
        assert!(!ws.path().join("sessions/scli001").exists());
        let repo = git2::Repository::open(ws.path()).unwrap();
        assert!(repo
            .find_branch("session/scli001", git2::BranchType::Local)
            .is_err());
    }

    #[test]
    fn discard_one_refuses_non_tty_without_yes() {
        let ws = ws_with_session("scli002");
        let err = discard_one(ws.path(), "scli002", false, false).unwrap_err();
        assert!(err.to_string().contains("--yes"));
        // Session must be untouched.
        assert!(ws.path().join("sessions/scli002").exists());
    }

    #[test]
    fn classify_puts_idle_unlocked_in_eligible() {
        let ws = ws_with_session("selig01");
        // create_session_worktree wrote no log.jsonl; write an empty one => Idle.
        std::fs::write(ws.path().join("sessions/selig01/log.jsonl"), "").unwrap();
        let (eligible, skipped) = classify_bulk_targets(ws.path()).unwrap();
        assert_eq!(eligible, vec!["selig01".to_string()]);
        assert!(skipped.is_empty());
    }

    #[test]
    fn classify_skips_running_session() {
        let ws = ws_with_session("srun01");
        std::fs::write(ws.path().join("sessions/srun01/log.jsonl"), "").unwrap();
        // Our own live PID in the lock => running.
        std::fs::write(
            ws.path().join("sessions/srun01/lock"),
            std::process::id().to_string(),
        )
        .unwrap();
        let (eligible, skipped) = classify_bulk_targets(ws.path()).unwrap();
        assert!(eligible.is_empty());
        assert_eq!(skipped, vec![("srun01".to_string(), "running".to_string())]);
    }

    #[test]
    fn classify_omits_dir_without_log() {
        let ws = ws_with_session("snolog");
        // No log.jsonl written => the explicit "missing log" guard in classify skips
        // it => omitted from both lists. (Missing file does NOT make session_state err.)
        let (eligible, skipped) = classify_bulk_targets(ws.path()).unwrap();
        assert!(!eligible.contains(&"snolog".to_string()));
        assert!(skipped.iter().all(|(id, _)| id != "snolog"));
    }

    #[test]
    fn classify_missing_sessions_dir_is_empty() {
        let ws = TempDir::new().unwrap(); // no sessions/ at all
        let (eligible, skipped) = classify_bulk_targets(ws.path()).unwrap();
        assert!(eligible.is_empty() && skipped.is_empty());
    }
}
