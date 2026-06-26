use clap::Parser;
use std::path::PathBuf;

use gait_cli::cli::args::{extract_path_arg, gait_dir_or_env};

fn cwd_or_dot() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
eprintln!("  gait -e \"<request>\"     one-off: ask the model; offer to run a command");

fn usage() -> ! {
    eprintln!("usage: gait <command> [options]");
    eprintln!("  run      [--session <id>] [--task <text>] [--gait-dir <path>] [--codebase <path>] [--auto] [--model <provider/model>] [--result-schema <file>] [--allow <pat>]... [--deny <pat>]... [--max-iter <n>]");
    eprintln!("  init     [<path>]");
    eprintln!("  finish   <session-id> [--gait-dir <path>]");
    eprintln!(
        "  undo    <session-id> [--turn N | --list] [--code] [--transcript] [--gait-dir <path>]"
    );
    eprintln!("  sessions [--gait-dir <path>]");
    eprintln!("  auth     <list|add|remove> [<provider>]");
    eprintln!("  models   [<filter>]");
    eprintln!("  workflow <run|list|continue|abort|log> [args...]");
    eprintln!(
        "  chat     [--gait-dir <path>] [--codebase <path>] [--agent <name>] [--session <id>]"
    );
    eprintln!("  allow    <session-id> [--pattern <pat>] [--scope <global|agent|once>] [--gait-dir <path>] [--agent <name>]  (was: approve)");
    eprintln!("  deny     <session-id> [--gait-dir <path>]                                                              (was: reject)");
    eprintln!("  unlock   <session-id> [--scope once|session|global|agent] [--agent <name>] [--gait-dir <path>]");
    eprintln!("  status   <session-id> [--gait-dir <path>] [--json]");
    eprintln!("  cost     <session-id> [--gait-dir <path>] [--json]");
    eprintln!("  register   <mcp|skill> <name> [flags] [--yes]            (was: load)");
    eprintln!("  unregister <mcp|skill> <name>                              (was: unload)");
    eprintln!("  enable     <mcp|skill> <name>");
    eprintln!("  disable    <mcp|skill> <name>");

    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    // Resolve gait_dir once for subcommands that need a config path (register/unregister/enable/disable).
    let gait_dir: PathBuf = extract_path_arg(&args[2..], "--gait-dir")
        .or_else(|| gait_dir_or_env(&args[2..]))
        .unwrap_or_else(cwd_or_dot);

    match args[1].as_str() {
        "-e" | "--exec" => gait_cli::cli::exec::cmd_exec(&args[2..]).await,
        "run" => gait_cli::cli::args::cmd_run(&args[2..]).await,

        "init" => gait_cli::cli::init::cmd_init(&args[2..]),
        "finish" => gait_cli::cli::session::cmd_finish(&args[2..]),
        "undo" => gait_cli::cli::session::cmd_undo(&args[2..]),
        "sessions" => gait_cli::cli::session::cmd_sessions(&args[2..]),
        "auth" => gait_cli::cli::admin::cmd_auth(&args[2..]),
        "models" => gait_cli::cli::admin::cmd_models(&args[2..]).await,
        "workflow" => gait_cli::cli::workflow::cmd_workflow(&args[2..]).await,
        "chat" => cmd_chat(&args[2..]).await,
        "allow" | "approve" => gait_cli::cli::session::cmd_allow(&args[2..]),
        "deny" | "reject" => gait_cli::cli::session::cmd_deny(&args[2..]),
        "unlock" => gait_cli::cli::session::cmd_unlock(&args[2..]),
        "status" => gait_cli::cli::session::cmd_status(&args[2..]),
        "cost" => gait_cli::cli::session::cmd_cost(&args[2..]),
        "register" | "load" => {
            let rest: Vec<String> = args[2..].to_vec();
            if let Err(e) = gait_cli::cli::admin::cmd_register(&gait_dir, &rest) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        "unregister" | "unload" => {
            let rest: Vec<String> = args[2..].to_vec();
            if let Err(e) = gait_cli::cli::admin::cmd_unregister(&gait_dir, &rest) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        "enable" => {
            let rest: Vec<String> = args[2..].to_vec();
            if let Err(e) = gait_cli::cli::admin::cmd_enable_disable(&gait_dir, &rest, true) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        "disable" => {
            let rest: Vec<String> = args[2..].to_vec();
            if let Err(e) = gait_cli::cli::admin::cmd_enable_disable(&gait_dir, &rest, false) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        "daemons" => {
            let rest: Vec<String> = args[2..].to_vec();
            gait_cli::cli::daemons::cmd_daemons(&gait_dir, &rest);
        }
        _ if gait_core::is_role_argv(&args[1..]) => {
            // gait-cli builds the GaitWorkflowRunner (avoids gait-bin needing gait-workflow dep).
            gait_cli::cli::args::cmd_dispatch_role(&args[1..]).await;
        }
        _ => {
            eprintln!("[gait] unknown command: {}", args[1]);
            usage();
        }
    }
}

#[derive(Parser)]
#[command(no_binary_name = true)]
struct ChatArgs {
    #[arg(long)]
    codebase: Option<PathBuf>,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    session: Option<String>,
    // Optional: falls back to pwd (chat is interactive, so a missing gait-dir is non-fatal).
    #[arg(long, env = "GAIT_DIR")]
    gait_dir: Option<PathBuf>,
    // Accepted to prevent "unexpected argument" errors. Container spawning is
    // handled externally — gait never self-spawns into a container.
    #[arg(long = "container", hide = true)]
    container: bool,
}

async fn cmd_chat(argv: &[String]) {
    let args = ChatArgs::parse_from(argv);

    // Same precedence as every other subcommand: --gait-dir arg > $GAIT_DIR env var > pwd.
    // Chat falls back to pwd rather than erroring because it's interactive.
    let gait_dir = args.gait_dir.unwrap_or_else(cwd_or_dot);

    let codebase = match args.codebase {
        Some(ref p) if p == std::path::Path::new(".") => cwd_or_dot(),
        Some(p) => p,
        None => cwd_or_dot(),
    };
    if !codebase.exists() {
        eprintln!(
            "[gait] warning: codebase path does not exist: {}",
            codebase.display()
        );
    }

    if let Err(e) = gait_repl::chat::run_chat(gait_dir, codebase, args.agent, args.session).await {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
}
