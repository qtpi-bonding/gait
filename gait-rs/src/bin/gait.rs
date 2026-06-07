use std::path::PathBuf;
use std::sync::Arc;
use gait_ai::{
    config::load_config,
    auth::resolve_api_key,
    factory::load_provider,
    tools::builtin_tools,
    turn::{TurnOpts, run_turn},
};

fn usage() -> ! {
    eprintln!("usage: gait run [--session <id>] [--task <text>] [--workspace <path>] [--codebase <path>] [--auto] [--model <provider/model>]");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { usage(); }

    match args[1].as_str() {
        "run" => cmd_run(&args[2..]).await,
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

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--session"   => { i += 1; session_id = argv.get(i).cloned(); }
            "--task"      => { i += 1; task = argv.get(i).cloned(); }
            "--workspace" => { i += 1; workspace = argv.get(i).map(PathBuf::from).unwrap_or(workspace); }
            "--codebase"  => { i += 1; codebase_path = argv.get(i).map(PathBuf::from); }
            "--model"     => { i += 1; model_override = argv.get(i).cloned(); }
            "--auto"      => { auto = true; }
            _ => {}
        }
        i += 1;
    }

    let session_id = session_id.unwrap_or_else(|| {
        let id = format!("s{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]);
        eprintln!("[gait] starting session {}", id);
        id
    });

    let config = load_config(&workspace);
    let model = model_override.as_deref().unwrap_or(&config.model).to_string();
    let codebase_path = codebase_path.unwrap_or_else(|| workspace.join("codebase"));

    let provider_name = model.split('/').next().unwrap_or("anthropic");
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

    // Append config-defined tools
    let mut tools = builtin_tools(codebase_path.clone());
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

    if let Err(e) = run_turn(
        TurnOpts {
            session_id,
            task,
            auto,
            workspace,
            codebase_path,
            allow: config.allow,
            system_prompt,
            model: model.clone(),
        },
        provider,
        tools,
    ).await {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
}
