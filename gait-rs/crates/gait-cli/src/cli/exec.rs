use clap::Parser;
use serde_json::{json, Value};
use std::path::Path;

/// `gait -e <prompt>` ephemeral one-off.
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct ExecArgs {
    /// Model override (provider/id)
    #[arg(short = 'm', long)]
    pub model: Option<String>,
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
    model_override: Option<String>,
    codebase: &Path,
) -> anyhow::Result<OneOff> {
    // model: override > config.model (pwd-fallback load) ; never panic, never a default.
    let model = match model_override {
        Some(m) => m,
        None => {
            // Entry point: surface gait.jsonc parse errors instead of silently
            // defaulting. `gait -e <prompt>` has no other config-loaded path;
            // silently defaulting here would let a broken `gait.jsonc` route
            // through a model the user never picked.
            match gait_core::config::load_config_with(
                codebase,
                gait_core::config::ConfigLoadPolicy::Strict,
            ) {
                Ok(c) => c.model.ok_or_else(|| {
                    anyhow::anyhow!("no model set; pass --model provider/id or set config.model")
                })?,
                Err(e) => return Err(e),
            }
        }
    };
    let provider = model
        .split_once('/')
        .map(|(p, _)| p)
        .ok_or_else(|| anyhow::anyhow!("model must be 'provider/model-id'"))?;
    let api_key = gait_core::auth::resolve_api_key(provider).ok_or_else(|| {
        anyhow::anyhow!("no API key for `{provider}`; run `gait auth add {provider}`")
    })?;
    let exe = std::env::current_exe()?;
    let res = gait_core::embed::run_one_off_turn(
        codebase,
        &api_key,
        &exe,
        &Default::default(),
        /* sandbox_on */ true,
        &model,
        prompt.to_string(),
        one_off_schema(),
    )
    .await?;
    let result = match res {
        gait_core::turn::TurnResult::Done {
            result: Some(r), ..
        } => r,
        _ => anyhow::bail!("model returned no structured result"),
    };
    parse_one_off(&result)
}

pub async fn cmd_exec(args: ExecArgs) {
    let model = args.model;
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
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let code = match run_one_off(&prompt, model, &cwd).await {
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
}
