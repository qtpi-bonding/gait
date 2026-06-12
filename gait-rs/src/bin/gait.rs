use std::path::PathBuf;
use std::sync::Arc;
use gait_ai::{
    config::load_config,
    auth::resolve_api_key,
    factory::load_provider,
    turn::run_turn,
    workflow::runner::{run_workflow, WorkflowRunOpts},
    Scheduler,
    HarnessBackend,
};

fn extract_path_arg(args: &[String], flag: &str) -> Option<std::path::PathBuf> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| std::path::PathBuf::from(&w[1]))
}

/// The single source of truth for gait-dir precedence: `--gait-dir` arg >
/// `$GAIT_DIR` env var > `None`. Callers pick the no-value policy (error vs.
/// fall back to pwd) — see `resolve_gait_dir` and `cmd_chat`.
fn gait_dir_or_env(args: &[String]) -> Option<std::path::PathBuf> {
    if let Some(p) = extract_path_arg(args, "--gait-dir") {
        return Some(p);
    }
    std::env::var("GAIT_DIR").ok().filter(|v| !v.is_empty()).map(std::path::PathBuf::from)
}

/// Resolve gait dir: --gait-dir arg > $GAIT_DIR env var > error.
fn resolve_gait_dir(args: &[String]) -> anyhow::Result<std::path::PathBuf> {
    gait_dir_or_env(args)
        .ok_or_else(|| anyhow::anyhow!("no gait dir: pass --gait-dir <path> or set $GAIT_DIR"))
}

/// Resolve codebase: --codebase arg > current working directory.
fn resolve_codebase(args: &[String]) -> std::path::PathBuf {
    extract_path_arg(args, "--codebase")
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
}

fn try_docker_route(orig_args: &[String]) {
    use std::io::IsTerminal;
    const IMAGE: &str = "gaitai/gait:latest";

    if !gait_ai::docker::docker_available() {
        eprintln!("[gait] Docker not available — running without sandbox. Install Docker for isolated execution.");
        return;
    }

    if !gait_ai::docker::image_present(IMAGE) {
        eprintln!("[gait] pulling {}...", IMAGE);
        if let Err(e) = gait_ai::docker::pull_image(IMAGE) {
            eprintln!("[gait] pull failed: {e} — running without sandbox.");
            return;
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let gait_dir = extract_path_arg(orig_args, "--gait-dir")
        .unwrap_or_else(|| cwd.clone())
        .canonicalize()
        .unwrap_or_else(|_| cwd.clone());
    let codebase = extract_path_arg(orig_args, "--codebase")
        .unwrap_or_else(|| gait_dir.clone())
        .canonicalize()
        .unwrap_or_else(|_| gait_dir.clone());

    let mounts = gait_ai::docker::build_mount_map(&gait_dir, &codebase);
    let _ = gait_ai::docker::write_mount_map(&gait_dir, &mounts);

    // Must match auth.rs: always ~/.local/share/gait (XDG convention, cross-platform)
    let auth_dir = dirs_next::home_dir().map(|h| h.join(".local/share/gait"));
    let interactive = std::io::stdin().is_terminal();
    let docker_args = gait_ai::docker::build_docker_args(
        IMAGE,
        &mounts,
        auth_dir.as_deref(),
        orig_args,
        interactive,
    );

    let status = std::process::Command::new("docker")
        .args(&docker_args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("[gait] docker exec failed: {e}");
            std::process::exit(1);
        });

    std::process::exit(status.code().unwrap_or(1));
}

fn usage() -> ! {
    eprintln!("usage: gait <command> [options]");
    eprintln!("  run      [--session <id>] [--task <text>] [--gait-dir <path>] [--codebase <path>] [--auto] [--model <provider/model>] [--container]");
    eprintln!("  init     [<repo-url>]");
    eprintln!("  finish   <session-id> [--gait-dir <path>]");
    eprintln!("  undo    <session-id> [--turn N | --list] [--code] [--transcript] [--gait-dir <path>]");
    eprintln!("  sessions [--gait-dir <path>]");
    eprintln!("  auth     <list|add|remove> [<provider>]");
    eprintln!("  models   [<filter>]");
    eprintln!("  workflow <run|list|continue|abort|log> [args...]");
    eprintln!("  chat     [--gait-dir <path>] [--codebase <path>] [--agent <name>] [--session <id>]");
    eprintln!("  approve  <session-id> [--pattern <pat>] [--scope <global|agent|once>] [--gait-dir <path>] [--agent <name>]");
    eprintln!("  reject   <session-id> [--gait-dir <path>]");
    eprintln!("  status   <session-id> [--gait-dir <path>] [--json]");
    eprintln!("  load     <mcp|skill> <name> [flags] [--yes]");
    eprintln!("  unload   <mcp|skill> <name>");

    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { usage(); }

    // Route through Docker only when --container is explicitly requested and we're not already inside one.
    let has_container = args[2..].iter().any(|a| a == "--container");
    if has_container && !gait_ai::docker::is_inside_container() {
        try_docker_route(&args[1..]); // calls process::exit if Docker succeeds
    }

    // Resolve gait_dir once for subcommands that need a config path (load/unload).
    let gait_dir: PathBuf = extract_path_arg(&args[2..], "--gait-dir")
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    match args[1].as_str() {
        "run"      => cmd_run(&args[2..]).await,
        "init"     => cmd_init(&args[2..]),
        "finish"   => cmd_finish(&args[2..]),
        "undo"   => cmd_undo(&args[2..]),
        "sessions" => cmd_sessions(&args[2..]),
        "auth"     => cmd_auth(&args[2..]),
        "models"   => cmd_models(&args[2..]).await,
        "workflow" => cmd_workflow(&args[2..]).await,
        "chat" => cmd_chat(&args[2..]).await,
        "approve"  => cmd_approve(&args[2..]),
        "reject"   => cmd_reject(&args[2..]),
        "status"   => cmd_status(&args[2..]),
        "load"     => {
            let rest: Vec<String> = args[2..].to_vec();
            if let Err(e) = cmd_load(&gait_dir, &rest) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        "unload"   => {
            let rest: Vec<String> = args[2..].to_vec();
            if let Err(e) = cmd_unload(&gait_dir, &rest) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        "enable"   => {
            let rest: Vec<String> = args[2..].to_vec();
            if let Err(e) = cmd_enable_disable(&gait_dir, &rest, true) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        "disable"  => {
            let rest: Vec<String> = args[2..].to_vec();
            if let Err(e) = cmd_enable_disable(&gait_dir, &rest, false) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("[gait] unknown command: {}", args[1]);
            usage();
        }
    }
}

async fn cmd_run(argv: &[String]) {
    let mut session_id: Option<String> = None;
    let mut task: Option<String> = None;
    let gait_dir = resolve_gait_dir(argv).unwrap_or_else(|e| {
        eprintln!("[gait] {e}");
        std::process::exit(1);
    });
    let codebase_path = resolve_codebase(argv);
    let mut auto = false;
    let mut model_override: Option<String> = None;
    let mut agent_name: Option<String> = None;

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--session"   => { i += 1; session_id = argv.get(i).cloned(); }
            "--task"      => { i += 1; task = argv.get(i).cloned(); }
            "--gait-dir" => { i += 1; }
            "--codebase"  => { i += 1; }
            "--model"     => { i += 1; model_override = argv.get(i).cloned(); }
            "--auto"      => { auto = true; }
            "--agent"     => { i += 1; agent_name = argv.get(i).cloned(); }
            _ => {}
        }
        i += 1;
    }

    let session_id = session_id.unwrap_or_else(|| {
        let id = format!("s{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]);
        println!("{}", serde_json::json!({"type": "session_id", "id": id}));
        id
    });

    let config = load_config(&gait_dir);

    // Resolve agent file: explicit --agent name, or fallback to agents/default/AGENT.md
    let agent_path: Option<PathBuf> = if let Some(ref name) = agent_name {
        Some(gait_ai::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_ai::agent_frontmatter::MarkdownKind::Agent,
            name,
        ))
    } else {
        let default = gait_ai::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_ai::agent_frontmatter::MarkdownKind::Agent,
            "default",
        );
        if default.exists() { Some(default) } else { None }
    };

    // Build the declarative profile (model precedence: --model > agent frontmatter > config),
    // then resolve it into a live provider + tools + MCP clients in one place.
    let profile = gait_ai::profile::Profile::from_config_and_agent(
        &config, &gait_dir, Some(codebase_path.as_path()), agent_path.as_deref(), model_override.clone());
    let model = profile.model.clone();

    let provider_name = match model.split_once('/') {
        Some((p, _)) => p,
        None => {
            eprintln!("[gait] error: model must be 'provider/model', got: '{}'", model);
            std::process::exit(1);
        }
    };
    let api_key = resolve_api_key(provider_name);

    let repos = gait_ai::git::Repos::production();

    let resolved = match profile.resolve(
        codebase_path.clone(),
        session_id.clone(),
        gait_dir.clone(),
        repos.clone(),
        api_key,
    ).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };

    // Expand $(cmd)/$(N) shell, @path files, and >(N) quote-reply in the task
    // string before the turn runs — same expander the REPL uses. Quote-reply
    // pulls the prior turn's blocks from the session log (empty for a fresh one).
    let task = task.map(|t| {
        let hist = gait_dir.join("tmp").join(format!("shell-{}.jsonl", &session_id));
        let events = gait_ai::session::ingestor::Ingestor::from_session(&gait_dir, &session_id)
            .events().unwrap_or_default();
        let last_blocks = gait_ai::session::retrace::last_turn_blocks(&events);
        gait_ai::repl_input::expand_input(&t, &gait_ai::repl_input::ExpandCtx {
            codebase: &codebase_path,
            history: &hist,
            last_blocks: &last_blocks,
        })
    });

    let session = gait_ai::session::Session::load(
        gait_dir.clone(),
        session_id.clone(),
        codebase_path,
        repos,
    );
    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<gait_ai::proto::gait::log::v1::StreamItem>();
    let emitter = gait_ai::Emitter::for_session(&gait_dir, &session_id, tx);

    let input = gait_ai::turn::TurnInput { task, auto, max_iter: config.max_iter };
    let turn_handle = tokio::spawn(async move {
        run_turn(&session, &emitter, resolved, input).await
        // emitter drops here → channel closes → rx.recv() yields None
    });

    use gait_ai::proto::gait::log::v1::stream_item::Item;
    while let Some(item) = rx.recv().await {
        match item.item {
            Some(Item::Event(ev)) => {
                if let Ok(json) = serde_json::to_string(&ev) { println!("{}", json); }
            }
            Some(Item::Delta(d)) => {
                println!("{}", serde_json::json!(
                    {"type": "text_delta", "content": d.content, "ts": gait_ai::session::transcript::now_ms()}));
            }
            None => {}
        }
    }

    let turn_result = match turn_handle.await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("[gait] error: turn task panicked: {}", e);
            std::process::exit(1);
        }
    };

    // Drive after-turn coordination via Scheduler (policy) + Harness.execute (mechanism).
    let sj = gait_ai::session::session_json::read_session_json(&gait_dir, &session_id);
    let scheduler = gait_ai::scheduler::NativeScheduler;
    let harness = gait_ai::harness::NativeHarnessBackend::new(gait_dir.clone());
    let ended = gait_ai::scheduler::TurnEnded {
        session_id: &session_id,
        assigned_by: sj.assigned_by.as_deref(),
        result: &turn_result,
    };
    for action in scheduler.next(&ended) {
        if let Err(e) = harness.execute(&action).await {
            eprintln!("[gait] warning: harness execute failed: {}", e);
        }
    }
}

/// Create the empty, editable layer affordances in the gait-dir (idempotent,
/// never clobbers) and ensure `USER.md` is gitignored.
fn scaffold_gait_dir(gait_dir: &std::path::Path) -> std::io::Result<()> {
    let agents = gait_dir.join("agents");
    std::fs::create_dir_all(&agents)?;
    for p in [agents.join("default.md"), gait_dir.join("USER.md")] {
        if !p.exists() { std::fs::write(&p, "")?; }
    }
    let gi_path = gait_dir.join(".gitignore");
    let mut gi = std::fs::read_to_string(&gi_path).unwrap_or_default();
    if !gi.lines().any(|l| l.trim() == "USER.md") {
        if !gi.is_empty() && !gi.ends_with('\n') { gi.push('\n'); }
        gi.push_str("USER.md\n");
        std::fs::write(&gi_path, gi)?;
    }
    Ok(())
}
fn cmd_init(argv: &[String]) {
    let repo_url = argv.first().filter(|a| !a.starts_with('-')).cloned();
    let gait_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    if let Err(e) = gait_ai::git::Repos::production().traces.init_workspace(&gait_dir) {
        eprintln!("[gait] git init failed: {}", e);
        std::process::exit(1);
    }
    let _ = scaffold_gait_dir(&gait_dir);

    let _ = std::fs::create_dir_all(gait_dir.join("sessions"));

    // .gitignore
    let gi_path = gait_dir.join(".gitignore");
    let gi_entry = "sessions/\n.gait-ws.lock\n";
    if !gi_path.exists() {
        let _ = std::fs::write(&gi_path, gi_entry);
    } else {
        let existing = std::fs::read_to_string(&gi_path).unwrap_or_default();
        if !existing.contains("sessions/") {
            let _ = std::fs::write(&gi_path, format!("{}\n{}", existing, gi_entry));
        }
    }

    // gait.jsonc
    let config_path = gait_dir.join("gait.jsonc");
    if !config_path.exists() {
        let _ = std::fs::write(&config_path, "{\n  // gait configuration\n  \"model\": \"anthropic/claude-sonnet-4-6\",\n  // Allow list: tools that run without approval. Default empty = everything gated.\n  \"allow\": [],\n  // Tier 1 config-defined tools: shell commands the LLM can call by name\n  \"tools\": []\n}\n");
    }

    // agents/ — gait_dir-local agent definitions (not tracked in git).
    // Each file: YAML frontmatter (model, allow, deny) + markdown body (additive system prompt).
    // gait-harness.md (built-in harness description) is always prepended automatically.
    let agents_dir = gait_dir.join("agents");
    let _ = std::fs::create_dir_all(&agents_dir);
    let default_agent = agents_dir.join("default.md");
    if !default_agent.exists() {
        let _ = std::fs::write(&default_agent, "# Default agent\n\nAdd project-specific instructions here. Appended to the built-in gait harness description.\n\nFrontmatter fields (optional):\n  model: anthropic/claude-sonnet-4-6\n  allow: [read, shell:git *]\n  deny: [shell:rm *]\n");
    }

    // Stage and initial commit
    let repo = match git2::Repository::open(&gait_dir) {
        Ok(r) => r,
        Err(e) => { eprintln!("[gait] cannot open repo: {e}"); std::process::exit(1); }
    };
    let sig = repo.signature()
        .or_else(|_| git2::Signature::now("gait", "gait@localhost"))
        .unwrap();
    let mut index = repo.index().unwrap();

    let to_stage = ["gait.jsonc", ".gitignore", "AGENTS.md"];
    for f in &to_stage {
        if gait_dir.join(f).exists() {
            let _ = index.add_path(std::path::Path::new(f));
        }
    }
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();

    let parent_commits: Vec<git2::Commit> = repo.head()
        .ok().and_then(|h| h.peel_to_commit().ok()).into_iter().collect();
    let parents: Vec<&git2::Commit> = parent_commits.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, "gait: init gait_dir", &tree, &parents).unwrap();

    if let Some(url) = repo_url {
        // Add submodule via git CLI (git2 submodule API is complex)
        let status = std::process::Command::new("git")
            .args(["submodule", "add", &url, "codebase"])
            .current_dir(&gait_dir)
            .status();
        match status {
            Ok(s) if s.success() => {
                eprintln!("[gait] gait_dir initialized with codebase submodule at {url}");
            }
            _ => eprintln!("[gait] warning: could not add submodule {url}"),
        }
    } else {
        eprintln!("[gait] gait_dir initialized");
        eprintln!("[gait] start an agent turn: gait run --task \"...\"");
    }
}
fn cmd_finish(argv: &[String]) {
    let mut session_id: Option<String> = None;
    let gait_dir = resolve_gait_dir(argv).unwrap_or_else(|e| {
        eprintln!("[gait] {e}");
        std::process::exit(1);
    });
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--gait-dir" => { i += 1; }
            other if !other.starts_with('-') => { session_id = Some(other.to_string()); }
            _ => {}
        }
        i += 1;
    }
    let session_id = match session_id {
        Some(id) => id,
        None => {
            eprintln!("usage: gait finish <session-id> [--gait-dir <path>]");
            std::process::exit(1);
        }
    };
    let repos = gait_ai::git::Repos::production();
    if let Err(e) = repos.traces.finish_session(&gait_dir, &session_id) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    if let Err(e) = repos.checkpoints.remove(&gait_dir, &session_id) {
        eprintln!("warning: failed to remove checkpoint db for {}: {}", session_id, e);
    }
    eprintln!("[gait] session {} merged into main", session_id);
}
fn cmd_undo(argv: &[String]) {
    let mut session_id: Option<String> = None;
    let mut turn: Option<u32> = None;
    let mut code = false;
    let mut transcript = false;
    let mut list = false;
    let gait_dir = resolve_gait_dir(argv).unwrap_or_else(|e| {
        eprintln!("[gait] {e}");
        std::process::exit(1);
    });

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--turn" => {
                i += 1;
                turn = Some(match argv.get(i) {
                    Some(v) => match v.parse() {
                        Ok(n) => n,
                        Err(e) => { eprintln!("[gait] error: --turn needs a numeric value: {}", e); std::process::exit(1); }
                    },
                    None => { eprintln!("[gait] error: --turn needs a value"); std::process::exit(1); }
                });
            }
            "--code" => code = true,
            "--transcript" => transcript = true,
            "--list" => list = true,
            "--gait-dir" => { i += 1; }
            other if !other.starts_with("--") && session_id.is_none() => {
                session_id = Some(other.to_string());
            }
            other => { eprintln!("[gait] error: unexpected argument: {}", other); std::process::exit(1); }
        }
        i += 1;
    }

    let session_id = match session_id {
        Some(id) => id,
        None => {
            eprintln!("usage: gait undo <session-id> [--turn N | --list] [--code] [--transcript] [--gait-dir <path>]");
            std::process::exit(1);
        }
    };

    if let Err(e) = undo_command(&session_id, turn, code, transcript, list, gait_dir) {
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
    gait_dir: std::path::PathBuf,
) -> anyhow::Result<()> {
    use gait_ai::session::transcript::read_events;
    use gait_ai::proto::gait::log::v1::{event, payload};

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
                let message = tc.output.as_ref().map(|o| o.message.clone()).unwrap_or_default();
                turns.push((idx, message, tc.checkpoint.clone()));
            }
        }
    }

    if list {
        for (idx, summary, cp) in &turns {
            let mark = cp.as_deref().map(|s| &s[..s.len().min(8)]).unwrap_or("--------");
            println!("turn {idx}  [{mark}]  {summary}");
        }
        return Ok(());
    }

    let turn = turn.ok_or_else(|| anyhow::anyhow!("--turn <n> is required (or use --list)"))?;
    let (_, _, checkpoint) = turns.iter()
        .find(|(idx, _, _)| *idx == turn)
        .ok_or_else(|| anyhow::anyhow!("no turn {} in session {}", turn, session_id))?
        .clone();

    // Default (neither flag) = both axes.
    let do_code = code || (!code && !transcript);
    let do_transcript = transcript || (!code && !transcript);

    let _lock = gait_ai::turn::SessionLock::acquire(&session_dir)
        .map_err(|e| anyhow::anyhow!("cannot undo: {} (is a turn in progress?)", e))?;

    if do_code {
        let codebase = gait_ai::session::session_json::read_session_json(&gait_dir, session_id)
            .codebase_path
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("session.json has no codebase_path; cannot undo code"))?;
        let repos = gait_ai::git::Repos::production();
        match checkpoint.as_deref() {
            Some(sha) => {
                repos.checkpoints.restore_to(&gait_dir, session_id, &codebase, sha)?;
                println!("code restored to turn {turn} ({})", &sha[..sha.len().min(8)]);
            }
            None => println!("turn {turn} has no code checkpoint (non-git codebase); code not undone"),
        }
    }

    if do_transcript {
        undo_transcript_to_turn(&gait_dir, session_id, turn)?;
        println!("transcript undone to turn {turn}");
    }
    Ok(())
}

fn undo_transcript_to_turn(gait_dir: &std::path::Path, session_id: &str, turn: u32) -> anyhow::Result<()> {
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
fn cmd_sessions(argv: &[String]) {
    let gait_dir = resolve_gait_dir(argv).unwrap_or_else(|e| {
        eprintln!("[gait] {e}");
        std::process::exit(1);
    });
    match gait_ai::git::Repos::production().traces.list_active_sessions(&gait_dir) {
        Ok(sessions) if sessions.is_empty() => println!("No active sessions."),
        Ok(sessions) => { for id in sessions { println!("{}", id); } }
        Err(e) => { eprintln!("[gait] error: {}", e); std::process::exit(1); }
    }
}

fn cmd_auth(argv: &[String]) {
    use gait_ai::auth;
    match argv.first().map(|s| s.as_str()) {
        None | Some("list") | Some("ls") => {
            let creds = auth::list_credentials();
            if creds.is_empty() {
                println!("No credentials configured. Run: gait auth add <provider>");
            } else {
                let mut sorted: Vec<_> = creds.into_iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                for (k, v) in sorted { println!("  {}: {}", k, v); }
            }
        }
        Some("add") => {
            let provider = match argv.get(1) {
                Some(p) => p.as_str(),
                None => { eprintln!("usage: gait auth add <provider>"); std::process::exit(1); }
            };
            eprint!("{} API key: ", provider);
            use std::io::BufRead;
            let key = std::io::BufReader::new(std::io::stdin())
                .lines().next()
                .and_then(|l| l.ok())
                .unwrap_or_default()
                .trim()
                .to_string();
            if key.is_empty() { eprintln!("empty key, aborted"); std::process::exit(1); }
            if let Err(e) = auth::write_api_key(provider, &key) {
                eprintln!("[gait] error: {}", e); std::process::exit(1);
            }
            println!("Saved {} credentials", provider);
        }
        Some("remove") | Some("rm") => {
            let provider = match argv.get(1) {
                Some(p) => p.as_str(),
                None => { eprintln!("usage: gait auth remove <provider>"); std::process::exit(1); }
            };
            if let Err(e) = auth::remove_api_key(provider) {
                eprintln!("[gait] error: {}", e); std::process::exit(1);
            }
            println!("Removed {} credentials", provider);
        }
        Some(other) => {
            eprintln!("unknown auth subcommand: {}", other);
            eprintln!("usage: gait auth <list|add|remove> [<provider>]");
            std::process::exit(1);
        }
    }
}

async fn cmd_models(argv: &[String]) {
    use gait_ai::models;
    let filter = argv.first().filter(|a| !a.starts_with('-')).map(|s| s.as_str());
    match models::get_models().await {
        Ok(models) => {
            let filtered = models::filter_models(&models, filter);
            for m in &filtered {
                println!("  {}/{}", m.provider, m.id);
            }
            println!("\n{} models{}",
                filtered.len(),
                filter.map(|f| format!(" matching \"{}\"", f)).unwrap_or_default());
        }
        Err(e) => { eprintln!("Error fetching models: {}", e); std::process::exit(1); }
    }
}
async fn cmd_workflow(argv: &[String]) {
    let gait_dir = resolve_gait_dir(argv).unwrap_or_else(|e| {
        eprintln!("[gait] {e}");
        std::process::exit(1);
    });
    let sub = argv.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "run" => cmd_workflow_run(&argv[1..], gait_dir).await,
        "list" => cmd_workflow_list(&argv[1..], gait_dir),
        "continue" => cmd_workflow_continue(&argv[1..], gait_dir).await,
        "abort" => cmd_workflow_abort(&argv[1..], gait_dir),
        "log" => cmd_workflow_log(&argv[1..], gait_dir),
        _ => {
            eprintln!("[gait] usage: gait workflow <run|list|continue|abort|log>");
            std::process::exit(1);
        }
    }
}

async fn cmd_workflow_run(argv: &[String], gait_dir: PathBuf) {
    let name = match argv.first() {
        Some(n) => n.clone(),
        None => { eprintln!("[gait] usage: gait workflow run <name> [args...]"); std::process::exit(1); }
    };
    let args: Vec<String> = argv[1..].to_vec();

    let yaml_path = gait_dir.join("workflows").join(format!("{}.yaml", name));
    let yaml_content = match std::fs::read_to_string(&yaml_path) {
        Ok(c) => c,
        Err(e) => { eprintln!("[gait] cannot read {}: {}", yaml_path.display(), e); std::process::exit(1); }
    };
    let wf: gait_ai::workflow::schema::WorkflowDef = match serde_yaml::from_str(&yaml_content) {
        Ok(w) => w,
        Err(e) => { eprintln!("[gait] invalid workflow YAML: {}", e); std::process::exit(1); }
    };

    let session_id = format!("wf{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]);
    eprintln!("[gait workflow] starting {} as session {}", name, session_id);
    let config = load_config(&gait_dir);
    let model = wf.model.clone().unwrap_or(config.model.clone());
    let provider_name = model.split('/').next().unwrap_or("minimax");
    let api_key = resolve_api_key(provider_name);
    let provider = match load_provider(&model, api_key) {
        Ok(p) => Arc::from(p),
        Err(e) => { eprintln!("[gait] provider error: {}", e); std::process::exit(1); }
    };

    if let Err(e) = run_workflow(WorkflowRunOpts {
        workflow_def: wf,
        yaml_path: yaml_path.to_string_lossy().to_string(),
        session_id,
        gait_dir: gait_dir.clone(),
        codebase: gait_dir.join("codebase"),
        provider,
        args,
    }).await {
        eprintln!("[gait workflow] error: {}", e);
        std::process::exit(1);
    }
}

fn cmd_workflow_list(_argv: &[String], gait_dir: PathBuf) {
    let sessions = gait_ai::git::Repos::production().traces.list_active_sessions(&gait_dir).unwrap_or_default();
    let mut found = false;
    for session_id in &sessions {
        if let Some(progress) = gait_ai::workflow::progress::read_progress(&gait_dir, session_id) {
            println!("{} — {} ({})", session_id, progress.name, progress.status);
            found = true;
        }
    }
    if !found { println!("(no active workflow sessions)"); }
}

async fn cmd_workflow_continue(argv: &[String], gait_dir: PathBuf) {
    let workflow_id = match argv.first() {
        Some(id) => id.clone(),
        None => { eprintln!("[gait] usage: gait workflow continue <workflow-id>"); std::process::exit(1); }
    };
    let progress = match gait_ai::workflow::progress::read_progress(&gait_dir, &workflow_id) {
        Some(p) => p,
        None => { eprintln!("[gait] no progress.yaml found for workflow {}", workflow_id); std::process::exit(1); }
    };
    let tx_path = gait_dir.join("sessions").join(&workflow_id).join("log.jsonl");
    let events = gait_ai::session::transcript::read_events(&tx_path).unwrap_or_default();
    let yaml_path_str = events.iter().find_map(|e| {
        use gait_ai::proto::gait::log::v1::{event, payload};
        if let Some(event::Body::Payload(p)) = &e.body {
            if let Some(payload::Kind::Raw(r)) = &p.kind {
                if r.r#type == "workflow_start" {
                    let v = r.payload.as_ref()
                        .and_then(|s| serde_json::to_value(s).ok())
                        .unwrap_or(serde_json::Value::Null);
                    return v.get("yaml_path").and_then(|x| x.as_str()).map(|s| s.to_string());
                }
            }
        }
        None
    }).unwrap_or_default();

    if yaml_path_str.is_empty() {
        eprintln!("[gait] cannot find workflow YAML path in transcript"); std::process::exit(1);
    }
    let yaml_content = match std::fs::read_to_string(&yaml_path_str) {
        Ok(c) => c,
        Err(e) => { eprintln!("[gait] cannot read {}: {}", yaml_path_str, e); std::process::exit(1); }
    };
    let wf: gait_ai::workflow::schema::WorkflowDef = match serde_yaml::from_str(&yaml_content) {
        Ok(w) => w,
        Err(e) => { eprintln!("[gait] invalid workflow YAML: {}", e); std::process::exit(1); }
    };

    eprintln!("[gait workflow] resuming {} ({})", progress.name, workflow_id);
    let config = load_config(&gait_dir);
    let model = wf.model.clone().unwrap_or(config.model.clone());
    let provider_name = model.split('/').next().unwrap_or("minimax");
    let api_key = resolve_api_key(provider_name);
    let provider = match load_provider(&model, api_key) {
        Ok(p) => Arc::from(p),
        Err(e) => { eprintln!("[gait] provider error: {}", e); std::process::exit(1); }
    };

    if let Err(e) = run_workflow(WorkflowRunOpts {
        workflow_def: wf,
        yaml_path: yaml_path_str,
        session_id: workflow_id,
        gait_dir: gait_dir.clone(),
        codebase: gait_dir.join("codebase"),
        provider,
        args: vec![],
    }).await {
        eprintln!("[gait workflow] error: {}", e);
        std::process::exit(1);
    }
}

fn cmd_workflow_abort(argv: &[String], gait_dir: PathBuf) {
    let workflow_id = match argv.first() {
        Some(id) => id.clone(),
        None => { eprintln!("[gait] usage: gait workflow abort <workflow-id>"); std::process::exit(1); }
    };
    let tx_path = gait_dir.join("sessions").join(&workflow_id).join("log.jsonl");
    let _ = gait_ai::proto::wire::emit(&tx_path, gait_ai::proto::wire::raw("workflow_error", serde_json::json!({
        "workflow_id": workflow_id.clone(),
        "failed_node": "(aborted)",
        "error": "user aborted",
    })));
    let _ = gait_ai::git::Repos::production().traces.commit_session(&gait_dir, &workflow_id, "workflow: aborted");
    println!("[gait workflow] aborted {}", workflow_id);
}

fn cmd_workflow_log(argv: &[String], gait_dir: PathBuf) {
    let workflow_id = match argv.first() {
        Some(id) => id.clone(),
        None => { eprintln!("[gait] usage: gait workflow log <workflow-id>"); std::process::exit(1); }
    };
    let tx_path = gait_dir.join("sessions").join(&workflow_id).join("log.jsonl");
    let events = gait_ai::session::transcript::read_events(&tx_path).unwrap_or_default();
    for event in &events {
        println!("{}", serde_json::to_string(event).unwrap_or_default());
    }
}

async fn cmd_chat(argv: &[String]) {
    let mut codebase_path: Option<PathBuf> = None;
    let mut agent_name: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--codebase" => { i += 1; codebase_path = argv.get(i).map(PathBuf::from); }
            "--agent"    => { i += 1; agent_name = argv.get(i).cloned(); }
            "--gait-dir" => { i += 1; }
            "--session"  => { i += 1; session_id = argv.get(i).cloned(); }
            _ => {}
        }
        i += 1;
    }

    // Same precedence as every other subcommand (via `gait_dir_or_env`): --gait-dir
    // arg > $GAIT_DIR env var > pwd. Chat falls back to pwd rather than erroring
    // because it's interactive.
    let gait_dir = gait_dir_or_env(argv)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let codebase = match codebase_path.as_deref().and_then(|p| p.to_str()) {
        Some(".") => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        _ => codebase_path.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
    };
    if !codebase.exists() {
        eprintln!("[gait] warning: codebase path does not exist: {}", codebase.display());
    }

    if let Err(e) = gait_ai::chat::run_chat(gait_dir, codebase, agent_name, session_id).await {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
}
fn cmd_approve(argv: &[String]) {
    use gait_ai::permission::{execute_approve, ApproveParams, ApproveScope};
    use gait_ai::agent_frontmatter::{resolve_markdown_context_path, MarkdownKind};

    let mut session_id: Option<String> = None;
    let mut pattern: Option<String> = None;
    let mut scope: Option<ApproveScope> = None;
    let mut agent_name: Option<String> = None;
    let gait_dir = resolve_gait_dir(argv).unwrap_or_else(|e| {
        eprintln!("[gait] {e}");
        std::process::exit(1);
    });

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--gait-dir" => { i += 1; }
            "--pattern"   => { i += 1; pattern = argv.get(i).cloned(); }
            "--scope"     => {
                i += 1;
                scope = match argv.get(i).map(|s| s.as_str()) {
                    Some("global") => Some(ApproveScope::Global),
                    Some("agent")  => Some(ApproveScope::Agent),
                    Some("once")   => Some(ApproveScope::Once),
                    Some(other) => {
                        eprintln!("[gait] error: unknown --scope '{}' (expected global|agent|once)", other);
                        std::process::exit(1);
                    }
                    None => {
                        eprintln!("[gait] error: --scope requires a value");
                        std::process::exit(1);
                    }
                };
            }
            "--agent"     => { i += 1; agent_name = argv.get(i).cloned(); }
            other if !other.starts_with('-') => { session_id = Some(other.to_string()); }
            _ => {}
        }
        i += 1;
    }

    let session_id = match session_id {
        Some(id) => id,
        None => {
            eprintln!("usage: gait approve <session-id> [--pattern <pat>] [--scope <global|agent|once>] [--gait-dir <path>] [--agent <name>]");
            std::process::exit(1);
        }
    };

    // Default scope is Once (single-turn only) when not specified.
    let scope = scope.unwrap_or(ApproveScope::Once);
    let agent_path = agent_name.as_deref()
        .map(|n| resolve_markdown_context_path(&gait_dir, MarkdownKind::Agent, n));

    let params = ApproveParams { scope, pattern };
    if let Err(e) = execute_approve(&gait_dir, &session_id, &params, agent_path.as_deref()) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    eprintln!("[gait] approved {} (scope: {:?})", session_id, params.scope);
}
fn cmd_reject(argv: &[String]) {
    use gait_ai::session::transcript::{session_state, write_permissions_reply, PermissionsReplyItem, SessionState};

    let mut session_id: Option<String> = None;
    let gait_dir = resolve_gait_dir(argv).unwrap_or_else(|e| {
        eprintln!("[gait] {e}");
        std::process::exit(1);
    });

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--gait-dir" => { i += 1; }
            other if !other.starts_with('-') => { session_id = Some(other.to_string()); }
            _ => {}
        }
        i += 1;
    }

    let session_id = match session_id {
        Some(id) => id,
        None => {
            eprintln!("usage: gait reject <session-id> [--gait-dir <path>]");
            std::process::exit(1);
        }
    };

    let tx_path = gait_dir.join("sessions").join(&session_id).join("log.jsonl");
    let state = match session_state(&tx_path) {
        Ok(s) => s,
        Err(e) => { eprintln!("[gait] error: {}", e); std::process::exit(1); }
    };

    let items = match state {
        SessionState::AwaitingPermission { items } => items,
        _ => {
            eprintln!("[gait] no pending permissions for session {}", session_id);
            std::process::exit(1);
        }
    };

    if items.is_empty() {
        eprintln!("[gait] no pending permissions for session {}", session_id);
        return;
    }

    let reply: Vec<PermissionsReplyItem> = items.iter().map(|it| PermissionsReplyItem {
        id: it.id.clone(),
        decision: "deny".to_string(),
        text: None,
        scope: None,
    }).collect();

    if let Err(e) = write_permissions_reply(&tx_path, "user", reply) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    eprintln!("[gait] rejected {} pending permission(s) for session {}", items.len(), session_id);
}
fn cmd_status(argv: &[String]) {
    use gait_ai::session::transcript::{session_state, SessionState};

    let mut session_id: Option<String> = None;
    let mut json = false;
    let gait_dir = resolve_gait_dir(argv).unwrap_or_else(|e| {
        eprintln!("[gait] {e}");
        std::process::exit(1);
    });

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--gait-dir" => { i += 1; }
            "--json"      => { json = true; }
            other if !other.starts_with('-') => { session_id = Some(other.to_string()); }
            _ => {}
        }
        i += 1;
    }

    let session_id = match session_id {
        Some(id) => id,
        None => {
            eprintln!("usage: gait status <session-id> [--gait-dir <path>] [--json]");
            std::process::exit(1);
        }
    };

    let tx_path = gait_dir.join("sessions").join(&session_id).join("log.jsonl");
    let state = match session_state(&tx_path) {
        Ok(s) => s,
        Err(e) => { eprintln!("[gait] error: {}", e); std::process::exit(1); }
    };

    if json {
        use gait_ai::proto::gait::state::v1 as sv1;
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
            }),
        };
        let report = sv1::SessionStateReport {
            session_id: session_id.clone(),
            state: Some(proto_state),
        };
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_else(|e| {
            eprintln!("[gait] error serializing state: {}", e);
            std::process::exit(1);
        }));
        return;
    }

    // Human-readable output
    match &state {
        SessionState::Idle => {
            println!("Session: {}", session_id);
            println!("State:   idle (no pending requests)");
        }
        SessionState::AwaitingPermission { items } => {
            println!("Session: {}", session_id);
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
            println!("Session: {}", session_id);
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
            println!("Session: {}", session_id);
            println!("State:   awaiting_inform ({} item(s))", items.len());
            for (n, it) in items.iter().enumerate() {
                println!("  [{}] id:    {}", n + 1, it.id);
                println!("      msg:   {}", it.content);
                if let Some(ctx) = &it.context {
                    println!("      ctx:   {}", ctx);
                }
            }
        }
    }
}

/// Parse --key value pairs from an arg slice into a HashMap.
fn parse_flags(args: &[String]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(key) = args[i].strip_prefix("--") {
            if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                map.insert(key.to_string(), args[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    map
}

fn read_config_as_value(config_path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let raw = std::fs::read_to_string(config_path).unwrap_or_else(|_| "{}".into());
    let stripped = gait_ai::config::strip_jsonc_comments(&raw);
    Ok(serde_json::from_str(&stripped).unwrap_or_default())
}

fn cmd_load(gait_dir: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    // Supported usage:
    //   gait load mcp <name> --spawn <cmd>
    //   gait load mcp <name> --connect <url>
    //   gait load skill <name> --path <file>
    //   gait load skill <name> --description <text> --content <text>
    // Adds to whitelist with enabled=false. Run 'gait enable <kind> <name>' to activate.

    let kind = args.get(0).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait load <mcp|skill> <name> [flags]"))?;
    let name = args.get(1).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait load <mcp|skill> <name> [flags]"))?;
    let flags = parse_flags(&args[2..]);

    let config_path = gait_dir.join("gait.jsonc");
    let mut val = read_config_as_value(&config_path)?;

    let entry = match kind {
        "mcp" => {
            if let Some(spawn) = flags.get("spawn") {
                serde_json::json!({ "name": name, "spawn": spawn, "enabled": false })
            } else if let Some(connect) = flags.get("connect") {
                serde_json::json!({ "name": name, "connect": connect, "enabled": false })
            } else {
                return Err(anyhow::anyhow!("gait load mcp: requires --spawn <cmd> or --connect <url>"));
            }
        }
        "skill" => {
            let description = flags.get("description")
                .ok_or_else(|| anyhow::anyhow!("gait load skill: requires --description <text>"))?;
            let content = if let Some(path) = flags.get("path") {
                std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("failed to read skill file '{}': {}", path, e))?
            } else if let Some(content) = flags.get("content") {
                content.clone()
            } else {
                return Err(anyhow::anyhow!("gait load skill: requires --path <file> or --content <text>"));
            };
            serde_json::json!({
                "name": name, "description": description,
                "content": content, "enabled": false
            })
        }
        other => return Err(anyhow::anyhow!("unknown kind '{}': use mcp or skill", other)),
    };

    let array_key = if kind == "mcp" { "mcp_servers" } else { "skills" };
    if val.get(array_key).is_none() {
        val[array_key] = serde_json::json!([]);
    }
    val[array_key].as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not an array", array_key))?
        .push(entry);

    eprintln!("Note: gait.jsonc comments are stripped on rewrite.");
    std::fs::write(&config_path, serde_json::to_string_pretty(&val)?)?;
    println!("Added {} '{}'.", kind, name);
    println!("→ Run 'gait enable {} {}' to activate it.", kind, name);
    Ok(())
}

fn cmd_enable_disable(gait_dir: &std::path::Path, args: &[String], enable: bool) -> anyhow::Result<()> {
    let kind = args.get(0).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait enable/disable <mcp|skill> <name>"))?;
    let name = args.get(1).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait enable/disable <mcp|skill> <name>"))?;

    let array_key = match kind {
        "mcp" => "mcp_servers",
        "skill" => "skills",
        other => return Err(anyhow::anyhow!("unknown kind '{}': use mcp or skill", other)),
    };

    let config_path = gait_dir.join("gait.jsonc");
    let mut val = read_config_as_value(&config_path)?;

    let arr = val[array_key].as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not an array", array_key))?;
    let entry = arr.iter_mut()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some(name))
        .ok_or_else(|| anyhow::anyhow!("{} '{}' not found in whitelist; run 'gait load {} {}' first", kind, name, kind, name))?;
    entry["enabled"] = serde_json::Value::Bool(enable);

    eprintln!("Note: gait.jsonc comments are stripped on rewrite.");
    std::fs::write(&config_path, serde_json::to_string_pretty(&val)?)?;
    eprintln!("{} {} '{}'.", if enable { "Enabled" } else { "Disabled" }, kind, name);
    Ok(())
}

fn cmd_unload(gait_dir: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    let kind = args.get(0).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait unload <mcp|skill> <name>"))?;
    let name = args.get(1).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait unload <mcp|skill> <name>"))?;

    let array_key = match kind {
        "mcp" => "mcp_servers",
        "skill" => "skills",
        other => return Err(anyhow::anyhow!("unknown kind '{}': use mcp or skill", other)),
    };

    let config_path = gait_dir.join("gait.jsonc");
    let mut val = read_config_as_value(&config_path)?;

    if let Some(arr) = val[array_key].as_array_mut() {
        arr.retain(|v| v.get("name").and_then(|n| n.as_str()) != Some(name));
    }

    eprintln!("Note: gait.jsonc comments are stripped on rewrite.");
    std::fs::write(&config_path, serde_json::to_string_pretty(&val)?)?;
    eprintln!("Removed {} '{}'.", kind, name);
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_path_arg_finds_value() {
        let args = vec![
            "run".to_string(),
            "--gait-dir".to_string(),
            "/foo/bar".to_string(),
        ];
        assert_eq!(
            extract_path_arg(&args, "--gait-dir"),
            Some(std::path::PathBuf::from("/foo/bar"))
        );
    }

    #[test]
    fn extract_path_arg_returns_none_when_missing() {
        let args = vec!["run".to_string(), "--task".to_string(), "hi".to_string()];
        assert_eq!(extract_path_arg(&args, "--gait-dir"), None);
    }

    #[test]
    fn extract_path_arg_returns_none_for_empty() {
        assert_eq!(extract_path_arg(&[], "--gait-dir"), None);
    }

    #[test]
    fn cmd_load_mcp_writes_entry_disabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"),
            r#"{"model":"anthropic/claude-sonnet-4-6","mcp_servers":[]}"#).unwrap();
        let args: Vec<String> = ["mcp", "slack", "--spawn", "node mcp-slack.js"]
            .iter().map(|s| s.to_string()).collect();
        cmd_load(dir.path(), &args).unwrap();
        let val: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap()).unwrap();
        assert_eq!(val["mcp_servers"][0]["name"], "slack");
        assert_eq!(val["mcp_servers"][0]["enabled"], false, "load should default to disabled");
    }

    #[test]
    fn cmd_load_skill_writes_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"),
            r#"{"model":"anthropic/claude-sonnet-4-6","skills":[]}"#).unwrap();
        let args: Vec<String> = ["skill", "tdd", "--description", "tdd workflow",
            "--content", "# TDD"].iter().map(|s| s.to_string()).collect();
        cmd_load(dir.path(), &args).unwrap();
        let val: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap()).unwrap();
        assert_eq!(val["skills"][0]["name"], "tdd");
        assert_eq!(val["skills"][0]["enabled"], false);
    }

    #[test]
    fn cmd_enable_sets_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[{"name":"slack","spawn":"node s.js","enabled":false}]}"#).unwrap();
        let args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
        cmd_enable_disable(dir.path(), &args, true).unwrap();
        let val: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap()).unwrap();
        assert_eq!(val["mcp_servers"][0]["enabled"], true);
    }

    #[test]
    fn cmd_disable_sets_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[{"name":"slack","spawn":"node s.js","enabled":true}]}"#).unwrap();
        let args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
        cmd_enable_disable(dir.path(), &args, false).unwrap();
        let val: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap()).unwrap();
        assert_eq!(val["mcp_servers"][0]["enabled"], false);
    }

    #[test]
    fn cmd_enable_missing_entry_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[]}"#).unwrap();
        let args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
        assert!(cmd_enable_disable(dir.path(), &args, true).is_err());
    }

    #[test]
    fn cmd_unload_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"),
            r#"{"model":"x","mcp_servers":[{"name":"slack","spawn":"node s.js","enabled":true}]}"#).unwrap();
        let args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
        cmd_unload(dir.path(), &args).unwrap();
        let val: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap()).unwrap();
        assert!(val["mcp_servers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn cmd_load_unknown_kind_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"x"}"#).unwrap();
        let args: Vec<String> = ["plugin", "foo"].iter().map(|s| s.to_string()).collect();
        assert!(cmd_load(dir.path(), &args).is_err());
    }

    #[test]
    fn scaffold_creates_empty_affordances_and_gitignores_user() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "sessions/\n").unwrap();
        scaffold_gait_dir(dir.path()).unwrap();
        assert!(dir.path().join("agents").join("default.md").exists());
        assert!(dir.path().join("USER.md").exists());
        let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gi.lines().any(|l| l.trim() == "USER.md"), "USER.md gitignored: {gi}");
        // Idempotent + non-clobbering:
        std::fs::write(dir.path().join("USER.md"), "mine").unwrap();
        scaffold_gait_dir(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("USER.md")).unwrap(), "mine");
        assert_eq!(gi.matches("USER.md").count(), 1, "no duplicate gitignore entry");
    }

    mod manifest_tests {
        use super::*;
        use serial_test::serial;

        #[test]
        #[serial]
        fn resolve_gait_dir_uses_env_var_when_no_arg() {
            std::env::set_var("GAIT_DIR", "/tmp/test-gait");
            let result = resolve_gait_dir(&[]);
            std::env::remove_var("GAIT_DIR");
            assert_eq!(result.unwrap(), std::path::PathBuf::from("/tmp/test-gait"));
        }

        #[test]
        #[serial]
        fn resolve_gait_dir_arg_overrides_env_var() {
            std::env::set_var("GAIT_DIR", "/tmp/env-gait");
            let args = vec!["--gait-dir".to_string(), "/tmp/arg-gait".to_string()];
            let result = resolve_gait_dir(&args);
            std::env::remove_var("GAIT_DIR");
            assert_eq!(result.unwrap(), std::path::PathBuf::from("/tmp/arg-gait"));
        }

        #[test]
        #[serial]
        fn resolve_gait_dir_errors_when_nothing_set() {
            std::env::remove_var("GAIT_DIR");
            let result = resolve_gait_dir(&[]);
            assert!(result.is_err(), "should error when no --gait-dir and no $GAIT_DIR");
        }

        #[test]
        fn resolve_codebase_defaults_to_pwd() {
            let result = resolve_codebase(&[]);
            assert_eq!(result, std::env::current_dir().unwrap());
        }

        #[test]
        fn resolve_codebase_uses_arg() {
            let args = vec!["--codebase".to_string(), "/tmp/mycode".to_string()];
            let result = resolve_codebase(&args);
            assert_eq!(result, std::path::PathBuf::from("/tmp/mycode"));
        }
    }
}
