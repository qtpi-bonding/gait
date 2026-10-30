use clap::Parser;
use serde_json::{json, Value};
use std::path::Path;
use std::process::ExitCode;

use crate::cli::error::CliError;

/// `gait -e <prompt>` ephemeral one-off.
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct ExecArgs {
    /// Named inference override.
    #[arg(long)]
    pub inference: Option<String>,
    /// Path to a caller-supplied structured output schema, overriding the
    /// default `{cmd|output}` one-off shape.
    #[arg(long)]
    pub result_schema: Option<std::path::PathBuf>,
    /// Inline JSON structured output schema, overriding the default
    /// `{cmd|output}` one-off shape.
    #[arg(long)]
    pub result_schema_json: Option<String>,
    /// The prompt (each positional is joined with spaces)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub prompt: Vec<String>,
}
#[derive(Debug, Clone, PartialEq)]
pub enum OneOff {
    Cmd(String),
    Output(String),
    /// A caller-supplied `--result-schema`/`--result-schema-json` was given:
    /// the raw schema-validated result, printed verbatim rather than
    /// interpreted as a shell command or plain-text answer.
    Raw(Value),
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
        _ => {
            return Err(CliError::failed(format!(
                "expected exactly one of `cmd` or `output`, got: {result}"
            )))
        }
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
            CliError::usage("no inference set; pass --inference <name> or set config.inference")
        })?;
    let selection = gait_ops::inference::resolve_selection(gait_dir, &name)?;
    if selection.inference.model.is_empty() {
        return Err(CliError::usage(format!(
            "inference '{name}' does not select a model"
        )));
    }
    Ok(selection.inference)
}

pub fn render_and_maybe_run(one_off: OneOff, is_tty: bool) -> anyhow::Result<i32> {
    match one_off {
        OneOff::Raw(value) => {
            println!("{}", serde_json::to_string(&value)?);
            Ok(0)
        }
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

/// Resolve `--result-schema`/`--result-schema-json` into a schema value.
/// `None` when neither flag was given — callers fall back to the default
/// `{cmd|output}` one-off shape.
fn resolve_custom_schema(args: &ExecArgs) -> anyhow::Result<Option<Value>> {
    if let Some(path) = &args.result_schema {
        let text = std::fs::read_to_string(path)
            .map_err(|e| {
                CliError::failed_with(&format!("reading --result-schema {}", path.display()), e)
            })?;
        return Ok(Some(serde_json::from_str(&text).map_err(|e| {
            CliError::invalid_input(format!("parsing --result-schema {}: {e}", path.display()))
        })?));
    }
    if let Some(raw) = &args.result_schema_json {
        return Ok(Some(serde_json::from_str(raw).map_err(|e| {
            CliError::invalid_input(format!("parsing --result-schema-json: {e}"))
        })?));
    }
    Ok(None)
}

pub async fn run_one_off(
    prompt: &str,
    inference_override: Option<String>,
    gait_dir: &Path,
    codebase: &Path,
    custom_schema: Option<Value>,
) -> anyhow::Result<OneOff> {
    // Resolve the same immutable named inference as persistent CLI/REPL turns.
    // The snapshot crosses the ephemeral boundary in memory; no selection
    // event or session is created.
    let inference = resolve_one_off_inference(gait_dir, inference_override.as_deref())?;
    let exe = std::env::current_exe()?;
    let using_custom_schema = custom_schema.is_some();
    let schema = custom_schema.unwrap_or_else(one_off_schema);
    let res = gait_core::embed::run_one_off_turn(
        codebase,
        &exe,
        &Default::default(),
        /* sandbox_on */ true,
        inference,
        prompt.to_string(),
        schema,
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
        .ok_or_else(|| CliError::failed("model returned no structured result"))?;
    if using_custom_schema {
        Ok(OneOff::Raw(result))
    } else {
        parse_one_off(&result)
    }
}

pub async fn cmd_exec(
    args: ExecArgs,
    gait_dir: &Path,
    codebase: &Path,
) -> anyhow::Result<ExitCode> {
    let custom_schema = resolve_custom_schema(&args)
        .map_err(|e| CliError::invalid_input(format!("gait -e: {e}")))?;
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
    match run_one_off(&prompt, inference, gait_dir, codebase, custom_schema).await {
        Ok(one) => {
            let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
            let code = render_and_maybe_run(one, is_tty).unwrap_or(1);
            Ok(ExitCode::from(code as u8))
        }
        Err(e) => Err(CliError::failed_with("gait -e", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_inference_is_a_usage_error_exit_65() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_one_off_inference(dir.path(), None).unwrap_err();
        assert_eq!(coded_error::cli::exit_code_anyhow(&err), 65);
    }

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

    // These cover the schema-resolution rejections, which return before cmd_exec
    // reads stdin, so the tests never touch the process's real stdin.
    #[tokio::test]
    async fn exec_with_unparseable_inline_schema_is_an_invalid_input_error() {
        let dir = tempfile::tempdir().unwrap();
        let args = ExecArgs {
            inference: None,
            result_schema: None,
            result_schema_json: Some("not json".to_string()),
            prompt: vec!["hi".to_string()],
        };
        let err = cmd_exec(args, dir.path(), dir.path())
            .await
            .expect_err("a non-JSON inline schema is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn exec_with_a_missing_schema_file_is_an_invalid_input_error() {
        let dir = tempfile::tempdir().unwrap();
        let args = ExecArgs {
            inference: None,
            result_schema: Some(dir.path().join("no-such-schema.json")),
            result_schema_json: None,
            prompt: vec!["hi".to_string()],
        };
        let err = cmd_exec(args, dir.path(), dir.path())
            .await
            .expect_err("a missing schema file is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn exec_with_a_schema_file_that_is_not_json_is_an_invalid_input_error() {
        let dir = tempfile::tempdir().unwrap();
        let schema = dir.path().join("schema.json");
        std::fs::write(&schema, "{ not json").unwrap();
        let args = ExecArgs {
            inference: None,
            result_schema: Some(schema),
            result_schema_json: None,
            prompt: vec!["hi".to_string()],
        };
        let err = cmd_exec(args, dir.path(), dir.path())
            .await
            .expect_err("a schema file that is not JSON is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::InvalidInput(_))
        ));
    }

    #[test]
    fn schema_has_oneof_two_variants() {
        let s = one_off_schema();
        assert_eq!(s["oneOf"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn resolve_custom_schema_none_when_neither_flag_set() {
        let args = ExecArgs {
            inference: None,
            result_schema: None,
            result_schema_json: None,
            prompt: vec![],
        };
        assert_eq!(resolve_custom_schema(&args).unwrap(), None);
    }

    #[test]
    fn resolve_custom_schema_parses_inline_json() {
        let args = ExecArgs {
            inference: None,
            result_schema: None,
            result_schema_json: Some(r#"{"type":"object"}"#.to_string()),
            prompt: vec![],
        };
        assert_eq!(
            resolve_custom_schema(&args).unwrap(),
            Some(json!({"type": "object"}))
        );
    }

    #[test]
    fn resolve_custom_schema_rejects_invalid_json() {
        let args = ExecArgs {
            inference: None,
            result_schema: None,
            result_schema_json: Some("not json".to_string()),
            prompt: vec![],
        };
        assert!(resolve_custom_schema(&args).is_err());
    }

    #[test]
    fn render_and_maybe_run_prints_raw_json_for_custom_schema_result() {
        let value = json!({"tests": {"a": {"body": "x"}}});
        // Only assert it doesn't error; stdout capture isn't worth the
        // plumbing here — `OneOff::Raw` just serializes and prints.
        assert_eq!(render_and_maybe_run(OneOff::Raw(value), false).unwrap(), 0);
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
            gait_dir.path().join("inferences/codex/inference.textproto"),
            "model: \"openai-codex/gpt-test\"\ncompaction_threshold_tokens: 128000\nllm_params { openai { reasoning_effort: \"high\" } }\n",
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
