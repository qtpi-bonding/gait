//! gait run command (extracted from bin/gait.rs).
use std::path::PathBuf;

#[derive(Default, Debug)]
pub struct RunArgs {
    pub session_id: Option<String>, pub task: Option<String>, pub auto: bool,
    pub model_override: Option<String>, pub agent_name: Option<String>,
    pub result_schema_path: Option<String>, pub allow: Vec<String>, pub deny: Vec<String>,
    pub max_iter: Option<usize>,
}
impl RunArgs {
    pub fn parse(argv: &[String]) -> Self {
        let mut a = RunArgs::default(); let mut i = 0;
        while i < argv.len() {
            match argv[i].as_str() {
                "--session" => { i += 1; a.session_id = argv.get(i).cloned(); }
                "--task" => { i += 1; a.task = argv.get(i).cloned(); }
                "--model" => { i += 1; a.model_override = argv.get(i).cloned(); }
                "--auto" => { a.auto = true; }
                "--agent" => { i += 1; a.agent_name = argv.get(i).cloned(); }
                "--result-schema" => { i += 1; a.result_schema_path = argv.get(i).cloned(); }
                "--allow" => { i += 1; if let Some(p) = argv.get(i) { a.allow.push(p.clone()); } }
                "--deny" => { i += 1; if let Some(p) = argv.get(i) { a.deny.push(p.clone()); } }
                "--max-iter" => { i += 1; a.max_iter = argv.get(i).and_then(|s| s.parse().ok()); }
                "--gait-dir" | "--codebase" => { i += 1; }
                _ => {}
            }
            i += 1;
        }
        a
    }
}

/// `gait run` — one turn, streaming to stdout. The bin owns path resolution and
/// passes the resolved `gait_dir` + `codebase_path` in; flags are parsed via
/// [`RunArgs`].
pub async fn cmd_run(argv: &[String], gait_dir: PathBuf, codebase_path: PathBuf) {
    use crate::port::output::{Diag, TurnSink};
    let args = RunArgs::parse(argv);

    // The single owned output boundary: Trace+Error stream to stderr, the final
    // RunResult prints once to stdout, diagnostics go to stderr (and set `failed`).
    let mut sink = crate::cli::output::StdioSink::stdio(false);

    // A fresh session announces its id on stdout (proto SessionStart). `--session`
    // resumes silently (the closure — and thus this line — never runs).
    let session_id = args.session_id.unwrap_or_else(|| {
        let id = format!("s{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]);
        sink.session_id(&id);
        id
    });

    let mut config = crate::config::load_config(&gait_dir);

    // Merge CLI --allow / --deny (additive: CLI entries extend, never replace, gait.jsonc lists).
    for p in args.allow { if !config.allow.contains(&p) { config.allow.push(p); } }
    for p in args.deny  { if !config.deny.contains(&p)  { config.deny.push(p);  } }
    if let Some(n) = args.max_iter { config.max_iter = Some(n); }

    // Apply --result-schema override: read file, validate JSON Schema, error early if invalid.
    if let Some(ref path) = args.result_schema_path {
        let raw = std::fs::read_to_string(path).unwrap_or_else(|e| {
            sink.diagnostic(Diag::Error, &format!("--result-schema: cannot read '{}': {}", path, e));
            std::process::exit(1);
        });
        let schema: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
            sink.diagnostic(Diag::Error, &format!("--result-schema: '{}' is not valid JSON: {}", path, e));
            std::process::exit(1);
        });
        jsonschema::JSONSchema::compile(&schema).unwrap_or_else(|e| {
            sink.diagnostic(Diag::Error, &format!("--result-schema: '{}' is not a valid JSON Schema: {}", path, e));
            std::process::exit(1);
        });
        config.result_schema = Some(schema);
    }

    // Resolve agent file: explicit --agent name, or fallback to agents/default/AGENT.md
    let agent_path: Option<PathBuf> = if let Some(ref name) = args.agent_name {
        Some(crate::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            crate::agent_frontmatter::MarkdownKind::Agent,
            name,
        ))
    } else {
        let default = crate::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            crate::agent_frontmatter::MarkdownKind::Agent,
            "default",
        );
        if default.exists() { Some(default) } else { None }
    };

    // Build the declarative profile (model precedence: --model > agent frontmatter > config),
    // then resolve it into a live provider + tools + MCP clients in one place.
    let profile = crate::profile::Profile::from_config_and_agent(
        &config, &gait_dir, Some(codebase_path.as_path()), agent_path.as_deref(), args.model_override.clone());
    let model = profile.model.clone();

    let provider_name = match model.split_once('/') {
        Some((p, _)) => p,
        None => {
            sink.diagnostic(Diag::Error, &format!("error: model must be 'provider/model', got: '{}'", model));
            std::process::exit(1);
        }
    };
    let api_key = crate::auth::resolve_api_key(provider_name);

    let repos = crate::git::Repos::production();

    // (Profile is resolved per-turn inside the CliDriver, below — the loop owns
    // that now. A resolve error surfaces as the run_loop's Err.)

    // Expand $(cmd)/$(N) shell, @path files, and >(N) quote-reply in the task
    // string before the turn runs — same expander the REPL uses. Quote-reply
    // pulls the prior turn's blocks from the session log (empty for a fresh one).
    let task = args.task.map(|t| {
        let hist = gait_dir.join("tmp").join(format!("shell-{}.jsonl", &session_id));
        let events = crate::session::ingestor::Ingestor::from_session(&gait_dir, &session_id)
            .events().unwrap_or_default();
        let last_blocks = crate::session::retrace::last_turn_blocks(&events);
        crate::repl_input::expand_input(&t, &crate::repl_input::ExpandCtx {
            codebase: &codebase_path,
            history: &hist,
            last_blocks: &last_blocks,
        })
    });

    // The primary input becomes an event the loop appends before the fold (or a
    // bare Resume when there's no --task) — symmetric with the reply path.
    let source = crate::port::input::ArgSource::new(
        task.map(|t| crate::embed::user_turn_body(&repos, &codebase_path, t)),
    );
    let session = std::sync::Arc::new(crate::session::Session::load(
        gait_dir.clone(),
        session_id.clone(),
        codebase_path.clone(),
        repos.clone(),
    ));
    let mut driver = CliDriver {
        profile: Some(profile),
        codebase: codebase_path,
        session_id: session_id.clone(),
        gait_dir: gait_dir.clone(),
        repos,
        api_key,
        auto: args.auto,
        max_iter: config.max_iter,
        preview_policy: config.preview,
    };

    if let Err(e) = crate::embed::run_loop(source, &mut sink, session, &gait_dir, &session_id, &mut driver).await {
        sink.diagnostic(Diag::Error, &format!("error: {}", e));
        std::process::exit(1);
    }

    if sink.failed { std::process::exit(1); }
}

/// CLI driver for `run_loop`: single-shot profile resolve, `--auto`/`--max-iter`
/// config, and the after-turn Scheduler/Harness coordination. A turn error sets
/// `failed` (via `diagnostic`) and breaks the loop → the post-loop `exit(1)`.
struct CliDriver {
    profile: Option<crate::profile::Profile>,
    codebase: PathBuf,
    session_id: String,
    gait_dir: PathBuf,
    repos: crate::git::Repos,
    api_key: Option<String>,
    auto: bool,
    max_iter: Option<usize>,
    preview_policy: crate::config::PreviewPolicy,
}

#[async_trait::async_trait(?Send)]
impl crate::embed::TurnDriver for CliDriver {
    async fn resolve_profile(&mut self) -> anyhow::Result<crate::profile::ResolvedProfile> {
        // Resolves a profile pre-built in cmd_run from flag-merged config
        // (--allow/--deny/--max-iter/--result-schema), so it can't use the plain
        // `embed::resolve_profile`. Single-shot: `ArgSource` yields one turn → resolves once.
        let profile = self.profile.take()
            .ok_or_else(|| anyhow::anyhow!("CLI driver resolve_profile() called more than once (single-shot)"))?;
        profile.resolve(
            self.codebase.clone(),
            self.session_id.clone(),
            self.gait_dir.clone(),
            self.repos.clone(),
            self.api_key.clone(),
        ).await
    }

    fn turn_config(&self) -> crate::embed::LoopConfig {
        crate::embed::LoopConfig { auto: self.auto, max_iter: self.max_iter, preview_policy: self.preview_policy }
    }

    async fn after_turn(&mut self, sink: &mut dyn crate::port::output::TurnSink, result: &crate::turn::TurnResult) {
        use crate::{HarnessBackend, Scheduler};
        let sj = crate::session::session_json::read_session_json(&self.gait_dir, &self.session_id);
        let scheduler = crate::scheduler::NativeScheduler;
        let harness = crate::harness::NativeHarnessBackend::new(self.gait_dir.clone());
        let ended = crate::scheduler::TurnEnded {
            session_id: &self.session_id,
            assigned_by: sj.assigned_by.as_deref(),
            result,
        };
        for action in scheduler.next(&ended) {
            if let Err(e) = harness.execute(&action).await {
                sink.diagnostic(crate::port::output::Diag::Info, &format!("warning: harness execute failed: {}", e));
            }
        }
    }

    fn on_turn_error(&mut self, sink: &mut dyn crate::port::output::TurnSink, e: anyhow::Error) -> std::ops::ControlFlow<()> {
        sink.diagnostic(crate::port::output::Diag::Error, &format!("error: {}", e));
        std::ops::ControlFlow::Break(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_extracts_task_and_flags() {
        let argv: Vec<String> = ["--task","hi","--auto","--model","x/y","--max-iter","7"].iter().map(|s| s.to_string()).collect();
        let a = RunArgs::parse(&argv);
        assert_eq!(a.task.as_deref(), Some("hi")); assert!(a.auto);
        assert_eq!(a.model_override.as_deref(), Some("x/y")); assert_eq!(a.max_iter, Some(7));
    }
    #[test]
    fn parse_no_task_is_none() { assert!(RunArgs::parse(&["--auto".to_string()]).task.is_none()); }
}
