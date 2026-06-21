//! Session lifecycle + inspection commands:
//! `finish`, `undo`, `sessions`, `allow|approve`, `deny|reject`, `status`, `cost`.

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(no_binary_name = true)]
struct FinishArgs {
    session_id: String,
    #[arg(long, env = "GAIT_DIR")]
    gait_dir: PathBuf,
}

pub fn cmd_finish(argv: &[String]) {
    let args = FinishArgs::parse_from(argv);
    let repos = gait_core::git::Repos::production();
    if let Err(e) = repos
        .traces
        .finish_session(&args.gait_dir, &args.session_id)
    {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    if let Err(e) = repos.checkpoints.remove(&args.gait_dir, &args.session_id) {
        eprintln!(
            "warning: failed to remove checkpoint db for {}: {}",
            args.session_id, e
        );
    }
    eprintln!("[gait] session {} merged into main", args.session_id);
}

#[derive(Parser)]
#[command(no_binary_name = true)]
struct UndoArgs {
    session_id: String,
    #[arg(long)]
    turn: Option<u32>,
    #[arg(long)]
    code: bool,
    #[arg(long)]
    transcript: bool,
    #[arg(long)]
    list: bool,
    #[arg(long, env = "GAIT_DIR")]
    gait_dir: PathBuf,
}

pub fn cmd_undo(argv: &[String]) {
    let args = UndoArgs::parse_from(argv);

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

#[derive(Parser)]
#[command(no_binary_name = true)]
struct SessionsArgs {
    #[arg(long, env = "GAIT_DIR")]
    gait_dir: PathBuf,
}

pub fn cmd_sessions(argv: &[String]) {
    let args = SessionsArgs::parse_from(argv);
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

#[derive(Parser)]
#[command(no_binary_name = true)]
struct AllowArgs {
    session_id: String,
    #[arg(long)]
    pattern: Option<String>,
    #[arg(long, value_name = "SCOPE")]
    scope: Option<String>,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long, env = "GAIT_DIR")]
    gait_dir: PathBuf,
}

pub fn cmd_allow(argv: &[String]) {
    use gait_core::agent_frontmatter::{resolve_markdown_context_path, MarkdownKind};
    use gait_core::permission::{compute_allow_decisions, AllowParams, AllowScope};
    use gait_core::session::log::{session_state, write_permissions_reply, SessionState};

    let args = AllowArgs::parse_from(argv);

    let scope = match args.scope.as_deref() {
        Some("global") => AllowScope::Global,
        Some("agent") => AllowScope::Agent,
        Some("once") | None => AllowScope::Once,
        Some(other) => {
            eprintln!(
                "[gait] error: unknown --scope '{}' (expected global|agent|once)",
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

#[derive(Parser)]
#[command(no_binary_name = true)]
struct DenyArgs {
    session_id: String,
    #[arg(long, env = "GAIT_DIR")]
    gait_dir: PathBuf,
}

pub fn cmd_deny(argv: &[String]) {
    use gait_core::proto::gait::log::v1 as pb;
    use gait_core::session::log::{session_state, write_permissions_reply, SessionState};

    let args = DenyArgs::parse_from(argv);

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

#[derive(Parser)]
#[command(no_binary_name = true)]
struct UnlockArgs {
    session_id: String,
    /// Grant scope: once (default, log only) | session (log only) | global (→ gait.jsonc) | agent (→ agent file)
    #[arg(long, default_value = "once")]
    scope: String,
    /// Agent name for agent-scoped grants (resolves to agents/<name>.md)
    #[arg(long)]
    agent: Option<String>,
    #[arg(long, env = "GAIT_DIR")]
    gait_dir: PathBuf,
}

pub fn cmd_unlock(argv: &[String]) {
    use gait_core::config::{
        append_sandbox_network_host, append_sandbox_readable, append_sandbox_writable,
    };
    use gait_core::proto::gait::log::v1 as pb;
    use gait_core::session::log::{session_state, write_unlock_reply, SessionState};

    let args = UnlockArgs::parse_from(argv);

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
        for item in &items {
            let result = match item.dimension.as_str() {
                "write" => append_sandbox_writable(&args.gait_dir, &item.resource),
                "read" => append_sandbox_readable(&args.gait_dir, &item.resource),
                "network" => append_sandbox_network_host(&args.gait_dir, &item.resource),
                other => {
                    eprintln!(
                        "[gait] warning: unknown dimension {:?}, skipping config write",
                        other
                    );
                    continue;
                }
            };
            if let Err(e) = result {
                eprintln!("[gait] warning: could not persist grant to config: {e}");
            }
        }
    } else if matches!(scope, pb::Scope::Agent) {
        // Agent-scoped: write to agent gait_dir sandbox block
        if let Some(agent_name) = &args.agent {
            use gait_core::agent_frontmatter::{resolve_markdown_context_path, MarkdownKind};
            let agent_path =
                resolve_markdown_context_path(&args.gait_dir, MarkdownKind::Agent, agent_name);
            if let Some(parent) = agent_path.parent() {
                for item in &items {
                    let result = match item.dimension.as_str() {
                        "write" => append_sandbox_writable(parent, &item.resource),
                        "read" => append_sandbox_readable(parent, &item.resource),
                        "network" => append_sandbox_network_host(parent, &item.resource),
                        _ => continue,
                    };
                    if let Err(e) = result {
                        eprintln!("[gait] warning: could not persist agent grant: {e}");
                    }
                }
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

#[derive(Parser)]
#[command(no_binary_name = true)]
struct StatusArgs {
    session_id: String,
    #[arg(long)]
    json: bool,
    #[arg(long, env = "GAIT_DIR")]
    gait_dir: PathBuf,
}

pub fn cmd_status(argv: &[String]) {
    use gait_core::session::log::{session_state, SessionState};

    let args = StatusArgs::parse_from(argv);

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

    if args.json {
        use gait_core::proto::gait::state::v1 as sv1;
        let proto_state = sv1::SessionState {
            state: Some(match &state {
                SessionState::Idle => sv1::session_state::State::Idle(sv1::Idle {}),
                SessionState::AwaitingPermission { items } => {
                    sv1::session_state::State::AwaitingPermission(sv1::AwaitingPermission {
                        items: items.clone(),
                    })
                }
                SessionState::AwaitingClarify { items } => {
                    sv1::session_state::State::AwaitingClarify(sv1::AwaitingClarify {
                        items: items.clone(),
                    })
                }
                SessionState::AwaitingInform { items } => {
                    sv1::session_state::State::AwaitingInform(sv1::AwaitingInform {
                        items: items.clone(),
                    })
                }
                SessionState::AwaitingUnlock { items } => {
                    sv1::session_state::State::AwaitingUnlock(sv1::AwaitingUnlock {
                        items: items.clone(),
                    })
                }
            }),
        };
        let report = sv1::SessionStateReport {
            session_id: args.session_id.clone(),
            state: Some(proto_state),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_else(|e| {
                eprintln!("[gait] error serializing state: {}", e);
                std::process::exit(1);
            })
        );
        return;
    }

    // Human-readable output
    match &state {
        SessionState::Idle => {
            println!("Session: {}", args.session_id);
            println!("State:   idle (no pending requests)");
        }
        SessionState::AwaitingPermission { items } => {
            println!("Session: {}", args.session_id);
            println!("State:   awaiting_permission ({} item(s))", items.len());
            for (n, it) in items.iter().enumerate() {
                println!("  [{}] id: {}", n + 1, it.id);
                if !it.tool_name.is_empty() {
                    println!("      tool: {}", it.tool_name);
                }
                println!("      cmd:  {}", it.cmd);
                if !it.reason.is_empty() {
                    println!("      reason: {}", it.reason);
                }
                if !it.patterns.is_empty() {
                    println!("      suggested patterns:");
                    for p in &it.patterns {
                        println!("        - {}", p);
                    }
                }
            }
        }
        SessionState::AwaitingClarify { items } => {
            println!("Session: {}", args.session_id);
            println!("State:   awaiting_clarify ({} item(s))", items.len());
            for (n, it) in items.iter().enumerate() {
                println!("  [{}] id:    {}", n + 1, it.id);
                println!("      kind:  {}", it.kind);
                println!("      Q:     {}", it.content);
                if !it.why.is_empty() {
                    println!("      why:   {}", it.why);
                }
            }
        }
        SessionState::AwaitingInform { items } => {
            println!("Session: {}", args.session_id);
            println!("State:   awaiting_inform ({} item(s))", items.len());
            for (n, it) in items.iter().enumerate() {
                println!("  [{}] id:    {}", n + 1, it.id);
                println!("      msg:   {}", it.content);
                if let Some(ctx) = &it.context {
                    println!("      ctx:   {}", ctx);
                }
            }
        }
        SessionState::AwaitingUnlock { items } => {
            println!("Session: {}", args.session_id);
            println!("State:   awaiting_unlock ({} item(s))", items.len());
            for (n, it) in items.iter().enumerate() {
                println!("  [{}] id:        {}", n + 1, it.id);
                if !it.tool_name.is_empty() {
                    println!("      tool:      {}", it.tool_name);
                }
                println!("      resource:  {}", it.resource);
                println!("      dimension: {}", it.dimension);
            }
        }
    }
}

#[derive(Parser)]
#[command(no_binary_name = true)]
struct CostArgs {
    session_id: String,
    #[arg(long)]
    json: bool,
    #[arg(long, env = "GAIT_DIR")]
    gait_dir: PathBuf,
}

pub fn cmd_cost(argv: &[String]) {
    use gait_core::proto::wire::read_events;
    use gait_core::session::retrace::total_usage;

    let args = CostArgs::parse_from(argv);

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
