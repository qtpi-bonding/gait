use std::path::PathBuf;
use std::sync::Arc;
use gait_ai::{
    config::load_config,
    auth::resolve_api_key,
    git,
    factory::load_provider,
    tools::builtin_tools,
    turn::{TurnOpts, run_turn},
    workflow::runner::{run_workflow, WorkflowRunOpts},
    HarnessBackend,
};

fn extract_path_arg(args: &[String], flag: &str) -> Option<std::path::PathBuf> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| std::path::PathBuf::from(&w[1]))
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
    let workspace = extract_path_arg(orig_args, "--workspace")
        .unwrap_or_else(|| cwd.clone())
        .canonicalize()
        .unwrap_or_else(|_| cwd.clone());
    let codebase = extract_path_arg(orig_args, "--codebase")
        .unwrap_or_else(|| workspace.clone())
        .canonicalize()
        .unwrap_or_else(|_| workspace.clone());

    let mounts = gait_ai::docker::build_mount_map(&workspace, &codebase);
    let _ = gait_ai::docker::write_mount_map(&workspace, &mounts);

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
    eprintln!("commands:");
    eprintln!("  run      [--session <id>] [--task <text>] [--workspace <path>] [--codebase <path>] [--auto] [--model <provider/model>] [--container]");
    eprintln!("  init     [<repo-url>]");
    eprintln!("  finish   <session-id> [--workspace <path>]");
    eprintln!("  sessions [--workspace <path>]");
    eprintln!("  auth     <list|add|remove> [<provider>]");
    eprintln!("  models   [<filter>]");
    eprintln!("  workflow <run|list|continue|abort|log> [args...]");
    eprintln!("  chat     [--workspace <path>] [--codebase <path>] [--agent <name>]");
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

    match args[1].as_str() {
        "run"      => cmd_run(&args[2..]).await,
        "init"     => cmd_init(&args[2..]),
        "finish"   => cmd_finish(&args[2..]),
        "sessions" => cmd_sessions(&args[2..]),
        "auth"     => cmd_auth(&args[2..]),
        "models"   => cmd_models(&args[2..]).await,
        "workflow" => cmd_workflow(&args[2..]).await,
        "chat" => cmd_chat(&args[2..]),
        _ => {
            eprintln!("[gait] unknown command: {}", args[1]);
            usage();
        }
    }
}

async fn cmd_run(argv: &[String]) {
    let mut session_id: Option<String> = None;
    let mut task: Option<String> = None;
    let mut workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut codebase_path: Option<PathBuf> = None;
    let mut auto = false;
    let mut model_override: Option<String> = None;
    let mut agent_name: Option<String> = None;

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--session"   => { i += 1; session_id = argv.get(i).cloned(); }
            "--task"      => { i += 1; task = argv.get(i).cloned(); }
            "--workspace" => { i += 1; workspace = argv.get(i).map(PathBuf::from).unwrap_or(workspace); }
            "--codebase"  => { i += 1; codebase_path = argv.get(i).map(PathBuf::from); }
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

    let config = load_config(&workspace);
    let model = model_override.as_deref().unwrap_or(&config.model).to_string();
    let codebase_path = codebase_path.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let provider_name = match model.split_once('/') {
        Some((p, _)) => p,
        None => {
            eprintln!("[gait] error: model must be 'provider/model', got: '{}'", model);
            std::process::exit(1);
        }
    };
    let api_key = resolve_api_key(provider_name);

    let provider = match load_provider(&model, api_key) {
        Ok(p) => Arc::from(p),
        Err(e) => {
            eprintln!("[gait] error: could not load provider for '{}': {}", model, e);
            std::process::exit(1);
        }
    };

    let agents_md = workspace.join("AGENTS.md");
    let system_prompt = std::fs::read_to_string(&agents_md).unwrap_or_else(|_| {
        "You are a helpful AI coding assistant. You have access to tools to read, write, and edit files in the codebase.".into()
    });

    // Discover tools from all configured MCP servers
    let mut mcp_registry: Vec<gait_ai::config::McpToolDef> = vec![];
    let mut mcp_clients: std::collections::HashMap<String, std::sync::Arc<gait_ai::mcp_client::McpClient>> = std::collections::HashMap::new();
    for server_def in &config.mcp_servers {
        match gait_ai::mcp_client::McpClient::connect(server_def).await {
            Ok(client) => {
                match client.discover_tools().await {
                    Ok(tools) => {
                        eprintln!("[gait] mcp {}: {} tool(s)", server_def.name, tools.len());
                        mcp_registry.extend(tools);
                        mcp_clients.insert(server_def.name.clone(), std::sync::Arc::new(client));
                    }
                    Err(e) => eprintln!("[gait] warning: mcp {} tools/list: {e}", server_def.name),
                }
            }
            Err(e) => eprintln!("[gait] warning: mcp {} connect: {e}", server_def.name),
        }
    }

    // Append config-defined tools
    let mut tools = builtin_tools(
        codebase_path.clone(),
        session_id.clone(),
        workspace.clone(),
        mcp_registry.clone(),
        config.skills.clone(),
    );
    for ct in &config.tools {
        let cmd = ct.cmd.clone();
        let cwd = if ct.cwd == "workspace" {
            workspace.clone()
        } else {
            codebase_path.clone()
        };
        let name = ct.name.clone();
        let description = ct.description.clone();
        // Note: config-defined tools receive no arguments from the model — they are fixed shell
        // commands. The empty parameters schema tells the model to call them with no input.
        tools.push(Box::new(gait_ai::types::ToolDef {
            name,
            description,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            execute: Box::new(move |_args| {
                let cmd = cmd.clone();
                let cwd = cwd.clone();
                Box::pin(async move {
                    let output = tokio::process::Command::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .current_dir(&cwd)
                        .output()
                        .await
                        .map_err(|e| anyhow::anyhow!("{}", e))?;
                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    let combined = if stderr.is_empty() { stdout } else { format!("{}\nstderr:\n{}", stdout, stderr) };
                    Ok(serde_json::Value::String(combined))
                })
            }),
        }) as Box<dyn gait_ai::types::Tool>);
    }

    // Merge agent-scoped allow patterns (from agents/<name>.md frontmatter) into config allow
    let mut effective_allow = config.allow;
    if let Some(ref name) = agent_name {
        let agent_path = gait_ai::agent_frontmatter::resolve_agent_path(&workspace, name);
        match gait_ai::agent_frontmatter::read_agent_allow(agent_path) {
            Ok(patterns) => effective_allow.extend(patterns),
            Err(e) => eprintln!("[gait] warning: could not read agent allow list: {}", e),
        }
    }

    let turn_result = match run_turn(
        TurnOpts {
            session_id: session_id.clone(),
            task,
            auto,
            workspace: workspace.clone(),
            codebase_path,
            allow: effective_allow,
            system_prompt,
            model: model.clone(),
            mcp_registry,
            mcp_clients,
            skill_registry: config.skills,
        },
        provider,
        tools,
    ).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
    };

    // Wire harness: inject agent_update into assigner + re-trigger if assigned_by is set
    let sj = gait_ai::session::session_json::read_session_json(&workspace, &session_id);
    let harness = gait_ai::harness::NativeHarnessBackend::new(workspace.clone());
    if let Err(e) = harness.after_turn(&session_id, sj.assigned_by.as_deref(), &turn_result).await {
        eprintln!("[gait] warning: harness after_turn failed: {}", e);
    }
}

fn cmd_init(argv: &[String]) {
    let repo_url = argv.first().filter(|a| !a.starts_with('-')).cloned();
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    if let Err(e) = git::init_workspace(&workspace) {
        eprintln!("[gait] git init failed: {}", e);
        std::process::exit(1);
    }

    let _ = std::fs::create_dir_all(workspace.join("sessions"));

    // .gitignore
    let gi_path = workspace.join(".gitignore");
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
    let config_path = workspace.join("gait.jsonc");
    if !config_path.exists() {
        let _ = std::fs::write(&config_path, "{\n  // gait configuration\n  \"model\": \"anthropic/claude-sonnet-4-6\",\n  // Allow list: tools that run without approval. Default empty = everything gated.\n  \"allow\": [],\n  // Tier 1 config-defined tools: shell commands the LLM can call by name\n  \"tools\": []\n}\n");
    }

    // AGENTS.md
    let agents_path = workspace.join("AGENTS.md");
    if !agents_path.exists() {
        let _ = std::fs::write(&agents_path, "# gait agent\n\nYou are a coding agent running inside gait, a stateless turn-based agent framework.\n\n**How it works:**\n- Each `gait run` invocation is one turn. Your full conversation history is replayed from a transcript each turn.\n- The codebase is the external repo you work on. The workspace holds gait config and session history.\n\n**Tools:**\n- `read`, `write`, `edit`, `glob`, `grep`, `ls`, `shell` — work on the codebase\n- Tools not in the allow list require user approval before running\n\n**Turn model:**\n- Complete your task in as few turns as possible\n- Read files rather than asking when you need information\n");
    }

    // Stage and initial commit
    let repo = match git2::Repository::open(&workspace) {
        Ok(r) => r,
        Err(e) => { eprintln!("[gait] cannot open repo: {e}"); std::process::exit(1); }
    };
    let sig = repo.signature()
        .or_else(|_| git2::Signature::now("gait", "gait@localhost"))
        .unwrap();
    let mut index = repo.index().unwrap();

    let to_stage = ["gait.jsonc", ".gitignore", "AGENTS.md"];
    for f in &to_stage {
        if workspace.join(f).exists() {
            let _ = index.add_path(std::path::Path::new(f));
        }
    }
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();

    let parent_commits: Vec<git2::Commit> = repo.head()
        .ok().and_then(|h| h.peel_to_commit().ok()).into_iter().collect();
    let parents: Vec<&git2::Commit> = parent_commits.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, "gait: init workspace", &tree, &parents).unwrap();

    if let Some(url) = repo_url {
        // Add submodule via git CLI (git2 submodule API is complex)
        let status = std::process::Command::new("git")
            .args(["submodule", "add", &url, "codebase"])
            .current_dir(&workspace)
            .status();
        match status {
            Ok(s) if s.success() => {
                eprintln!("[gait] workspace initialized with codebase submodule at {url}");
            }
            _ => eprintln!("[gait] warning: could not add submodule {url}"),
        }
    } else {
        eprintln!("[gait] workspace initialized");
        eprintln!("[gait] start an agent turn: gait run --task \"...\"");
    }
}

fn cmd_finish(argv: &[String]) {
    let mut session_id: Option<String> = None;
    let mut workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--workspace" => { i += 1; if let Some(p) = argv.get(i) { workspace = std::path::PathBuf::from(p); } }
            other if !other.starts_with('-') => { session_id = Some(other.to_string()); }
            _ => {}
        }
        i += 1;
    }
    let session_id = match session_id {
        Some(id) => id,
        None => {
            eprintln!("usage: gait finish <session-id> [--workspace <path>]");
            std::process::exit(1);
        }
    };
    if let Err(e) = git::finish_session(&workspace, &session_id) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    eprintln!("[gait] session {} merged into main", session_id);
}

fn cmd_sessions(argv: &[String]) {
    let mut workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--workspace" { i += 1; if let Some(p) = argv.get(i) { workspace = std::path::PathBuf::from(p); } }
        i += 1;
    }
    match git::list_active_sessions(&workspace) {
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
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let sub = argv.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "run" => cmd_workflow_run(&argv[1..], workspace).await,
        "list" => cmd_workflow_list(&argv[1..], workspace),
        "continue" => cmd_workflow_continue(&argv[1..], workspace).await,
        "abort" => cmd_workflow_abort(&argv[1..], workspace),
        "log" => cmd_workflow_log(&argv[1..], workspace),
        _ => {
            eprintln!("[gait] usage: gait workflow <run|list|continue|abort|log>");
            std::process::exit(1);
        }
    }
}

async fn cmd_workflow_run(argv: &[String], workspace: PathBuf) {
    let name = match argv.first() {
        Some(n) => n.clone(),
        None => { eprintln!("[gait] usage: gait workflow run <name> [args...]"); std::process::exit(1); }
    };
    let args: Vec<String> = argv[1..].to_vec();

    let yaml_path = workspace.join("workflows").join(format!("{}.yaml", name));
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
    let config = load_config(&workspace);
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
        workspace: workspace.clone(),
        codebase: workspace.join("codebase"),
        provider,
        args,
    }).await {
        eprintln!("[gait workflow] error: {}", e);
        std::process::exit(1);
    }
}

fn cmd_workflow_list(_argv: &[String], workspace: PathBuf) {
    let sessions = gait_ai::git::list_active_sessions(&workspace).unwrap_or_default();
    let mut found = false;
    for session_id in &sessions {
        if let Some(progress) = gait_ai::workflow::progress::read_progress(&workspace, session_id) {
            println!("{} — {} ({})", session_id, progress.name, progress.status);
            found = true;
        }
    }
    if !found { println!("(no active workflow sessions)"); }
}

async fn cmd_workflow_continue(argv: &[String], workspace: PathBuf) {
    let workflow_id = match argv.first() {
        Some(id) => id.clone(),
        None => { eprintln!("[gait] usage: gait workflow continue <workflow-id>"); std::process::exit(1); }
    };
    let progress = match gait_ai::workflow::progress::read_progress(&workspace, &workflow_id) {
        Some(p) => p,
        None => { eprintln!("[gait] no progress.yaml found for workflow {}", workflow_id); std::process::exit(1); }
    };
    let tx_path = workspace.join("sessions").join(&workflow_id).join("transcript.jsonl");
    let events = gait_ai::session::transcript::read_events(&tx_path).unwrap_or_default();
    let yaml_path_str = events.iter().find_map(|e| {
        if let gait_ai::session::transcript::AnyEvent::WorkflowStart { yaml_path, .. } = e {
            Some(yaml_path.clone())
        } else { None }
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
    let config = load_config(&workspace);
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
        workspace: workspace.clone(),
        codebase: workspace.join("codebase"),
        provider,
        args: vec![],
    }).await {
        eprintln!("[gait workflow] error: {}", e);
        std::process::exit(1);
    }
}

fn cmd_workflow_abort(argv: &[String], workspace: PathBuf) {
    let workflow_id = match argv.first() {
        Some(id) => id.clone(),
        None => { eprintln!("[gait] usage: gait workflow abort <workflow-id>"); std::process::exit(1); }
    };
    let tx_path = workspace.join("sessions").join(&workflow_id).join("transcript.jsonl");
    let _ = gait_ai::session::transcript::emit_event(&tx_path, &gait_ai::session::transcript::AnyEvent::WorkflowError {
        workflow_id: workflow_id.clone(),
        failed_node: "(aborted)".into(),
        error: "user aborted".into(),
        ts: gait_ai::session::transcript::now_ms(),
    });
    let _ = gait_ai::git::commit_session(&workspace, &workflow_id, "workflow: aborted");
    println!("[gait workflow] aborted {}", workflow_id);
}

fn cmd_workflow_log(argv: &[String], workspace: PathBuf) {
    let workflow_id = match argv.first() {
        Some(id) => id.clone(),
        None => { eprintln!("[gait] usage: gait workflow log <workflow-id>"); std::process::exit(1); }
    };
    let tx_path = workspace.join("sessions").join(&workflow_id).join("transcript.jsonl");
    let events = gait_ai::session::transcript::read_events(&tx_path).unwrap_or_default();
    for event in &events {
        println!("{}", serde_json::to_string(event).unwrap_or_default());
    }
}

fn cmd_chat(argv: &[String]) {
    let mut workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut codebase_path: Option<PathBuf> = None;
    let mut agent_name: Option<String> = None;

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--workspace" => { i += 1; workspace = argv.get(i).map(PathBuf::from).unwrap_or(workspace); }
            "--codebase"  => { i += 1; codebase_path = argv.get(i).map(PathBuf::from); }
            "--agent"     => { i += 1; agent_name = argv.get(i).cloned(); }
            _ => {}
        }
        i += 1;
    }

    let codebase = match codebase_path.as_deref().and_then(|p| p.to_str()) {
        Some(".") => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        _ => codebase_path.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
    };
    if !codebase.exists() {
        eprintln!("[gait] warning: codebase path does not exist: {}", codebase.display());
    }

    if let Err(e) = gait_ai::chat::run_chat(workspace, codebase, agent_name) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_path_arg_finds_value() {
        let args = vec![
            "run".to_string(),
            "--workspace".to_string(),
            "/foo/bar".to_string(),
        ];
        assert_eq!(
            extract_path_arg(&args, "--workspace"),
            Some(std::path::PathBuf::from("/foo/bar"))
        );
    }

    #[test]
    fn extract_path_arg_returns_none_when_missing() {
        let args = vec!["run".to_string(), "--task".to_string(), "hi".to_string()];
        assert_eq!(extract_path_arg(&args, "--workspace"), None);
    }

    #[test]
    fn extract_path_arg_returns_none_for_empty() {
        assert_eq!(extract_path_arg(&[], "--workspace"), None);
    }
}
