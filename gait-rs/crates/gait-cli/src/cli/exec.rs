use clap::Parser;
use serde_json::{json, Value};
use std::path::Path;

/// `gait -e <prompt>` ephemeral one-off.
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct ExecArgs {
    /// Named inference override.
    #[arg(long)]
    pub inference: Option<String>,
    /// The prompt (each positional is joined with spaces)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub prompt: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneOff {
    Cmd(String),
    Output(String),
}

/// Two-variant mutually-exclusive schema: exactly one of `cmd` | `output`.
pub fn one_off_schema() -> Value {
    let str_obj = |key: &str| {
        json!({
            "type": "object",
            "required": [key],
            "additionalProperties": false,
            "properties": { key: { "type": "string" } }
        })
    };
    json!({ "oneOf": [ str_obj("cmd"), str_obj("output") ] })
}

pub fn parse_one_off(result: &Value) -> anyhow::Result<OneOff> {
    let cmd = result.get("cmd").and_then(Value::as_str);
    let out = result.get("output").and_then(Value::as_str);
    match (cmd, out) {
        (Some(c), None) => Ok(OneOff::Cmd(c.to_string())),
        (None, Some(o)) => Ok(OneOff::Output(o.to_string())),
        _ => anyhow::bail!("expected exactly one of `cmd` or `output`, got: {result}"),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RunDecision {
    Run,
    Quit,
}

/// `y`/`Y` → run; Enter → no-op (redraw); anything else (incl. `q`/EOF) → quit.
pub fn decide_key(key: char) -> Option<RunDecision> {
    match key {
        'y' | 'Y' => Some(RunDecision::Run),
        '\n' | '\r' => None,
        _ => Some(RunDecision::Quit),
    }
}

fn resolve_one_off_inference(
    gait_dir: &Path,
    inference_override: Option<&str>,
) -> anyhow::Result<gait_core::proto::gait::log::v1::Inference> {
    let config =
        gait_core::config::load_config_with(gait_dir, gait_core::config::ConfigLoadPolicy::Strict)?;
    let name = inference_override
        .map(str::to_owned)
        .or(config.inference)
        .ok_or_else(|| {
            anyhow::anyhow!("no inference set; pass --inference <name> or set config.inference")
        })?;
    let selection = gait_ops::inference::resolve_selection(gait_dir, &name)?;
    if selection.inference.model.is_empty() {
        anyhow::bail!("inference '{name}' does not select a model");
    }
    Ok(selection.inference)
}

pub fn render_and_maybe_run(one_off: OneOff, is_tty: bool) -> anyhow::Result<i32> {
    match one_off {
        OneOff::Output(text) => {
            println!("{text}");
            Ok(0)
        }
        OneOff::Cmd(cmd) => {
            println!("{cmd}");
            if !is_tty {
                return Ok(0); // piped: print only, scriptable
            }
            loop {
                eprint!("run? [y/N] ");
                let key = read_single_key()?;
                match decide_key(key) {
                    None => continue, // Enter: redraw the prompt
                    Some(RunDecision::Quit) => return Ok(0),
                    Some(RunDecision::Run) => {
                        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                        let status = std::process::Command::new(shell)
                            .arg("-c")
                            .arg(&cmd)
                            .status()?;
                        return Ok(status.code().unwrap_or(1));
                    }
                }
            }
        }
    }
}

/// Read a single key in raw mode (one byte from the controlling TTY).
fn read_single_key() -> anyhow::Result<char> {
    use std::io::Read;
    let mut buf = [0u8; 1];
    std::io::stdin().read_exact(&mut buf)?;
    Ok(buf[0] as char)
}

pub async fn run_one_off(
    prompt: &str,
    inference_override: Option<String>,
    gait_dir: &Path,
    codebase: &Path,
) -> anyhow::Result<OneOff> {
    // Resolve the same immutable named inference as persistent CLI/REPL turns.
    // The snapshot crosses the ephemeral boundary in memory; no selection
    // event or session is created.
    let inference = resolve_one_off_inference(gait_dir, inference_override.as_deref())?;
    let exe = std::env::current_exe()?;
    let res = gait_core::embed::run_one_off_turn(
        codebase,
        &exe,
        &Default::default(),
        /* sandbox_on */ true,
        inference,
        prompt.to_string(),
        one_off_schema(),
    )
    .await?;
    let result = res
        .turns
        .last()
        .and_then(|turn| turn.outcome.as_ref())
        .and_then(|outcome| match outcome {
            gait_core::proto::gait::log::v1::turn_result::Outcome::Done(done) => {
                done.output.as_ref()
            }
            _ => None,
        })
        .and_then(|output| match output {
            gait_core::proto::gait::log::v1::turn_done::Output::Structured(output) => {
                output.result.as_ref()
            }
            _ => None,
        })
        .and_then(|value| serde_json::to_value(value).ok())
        .ok_or_else(|| anyhow::anyhow!("model returned no structured result"))?;
    parse_one_off(&result)
}

pub async fn cmd_exec(args: ExecArgs, gait_dir: &Path, codebase: &Path) {
    let inference = args.inference;
    let mut prompt = args.prompt.join(" ");
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        use std::io::Read;
        let mut piped = String::new();
        let _ = std::io::stdin().read_to_string(&mut piped);
        if !piped.trim().is_empty() {
            if !prompt.is_empty() {
                prompt.push('\n');
            }
            prompt.push_str(piped.trim_end());
        }
    }
    let code = match run_one_off(&prompt, inference, gait_dir, codebase).await {
        Ok(one) => {
            let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
            render_and_maybe_run(one, is_tty).unwrap_or(1)
        }
        Err(e) => {
            eprintln!("gait -e: {e}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_cmd_variant() {
        let v = json!({ "cmd": "tar xf a.tar" });
        assert!(matches!(parse_one_off(&v).unwrap(), OneOff::Cmd(c) if c == "tar xf a.tar"));
    }

    #[test]
    fn parses_output_variant() {
        let v = json!({ "output": "Paris" });
        assert!(matches!(parse_one_off(&v).unwrap(), OneOff::Output(o) if o == "Paris"));
    }

    #[test]
    fn rejects_both_or_neither() {
        assert!(parse_one_off(&json!({ "cmd": "x", "output": "y" })).is_err());
        assert!(parse_one_off(&json!({})).is_err());
    }

    #[test]
    fn schema_has_oneof_two_variants() {
        let s = one_off_schema();
        assert_eq!(s["oneOf"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn enter_is_noop_y_runs() {
        assert_eq!(decide_key('y'), Some(RunDecision::Run));
        assert_eq!(decide_key('Y'), Some(RunDecision::Run));
        assert_eq!(decide_key('\n'), None); // Enter: no-op
        assert_eq!(decide_key('\r'), None);
        assert_eq!(decide_key('q'), Some(RunDecision::Quit));
        assert_eq!(decide_key('x'), Some(RunDecision::Quit)); // any other key quits
    }

    #[test]
    fn one_off_resolves_named_snapshot_from_gait_workspace_with_params() {
        let gait_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(gait_dir.path().join("inferences/codex")).unwrap();
        std::fs::write(
            gait_dir.path().join("inferences/codex/INFERENCE.yaml"),
            "model: openai-codex/gpt-test\ncompaction_threshold_tokens: 128000\nllm_params:\n  openai:\n    reasoning_effort: high\n",
        )
        .unwrap();
        std::fs::write(
            gait_dir.path().join("gait.jsonc"),
            r#"{"inference":"missing-default"}"#,
        )
        .unwrap();

        let inference = resolve_one_off_inference(gait_dir.path(), Some("codex")).unwrap();
        assert_eq!(inference.name, "codex");
        assert_eq!(inference.model, "openai-codex/gpt-test");
        assert!(inference.llm_params.is_some());
        assert!(!gait_dir.path().join(".active").exists());
    }
}
