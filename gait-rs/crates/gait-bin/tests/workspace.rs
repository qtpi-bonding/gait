//! `init`, `link` and `manual` on the registry: the gait dir is `--gait-dir`,
//! never a positional; output is camelCase JSON; failures carry their code.
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn gait(cwd: &Path, args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_gait"))
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("GAIT_DIR")
        .output()
        .unwrap();
    Run {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn json(text: &str) -> Value {
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn error_code(run: &Run) -> String {
    let last = run.stderr.lines().last().unwrap_or("");
    json(last)["error"]["code"].as_str().unwrap_or_default().to_owned()
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn init_creates_a_gait_dir_and_prints_camel_case_json() {
    let cwd = tempfile::tempdir().unwrap();
    let dir = cwd.path().join("home");
    let run = gait(cwd.path(), &["init", "--gait-dir", s(&dir)]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let out = json(&run.stdout);
    assert_eq!(out["gaitDir"], s(&dir));
    assert_eq!(out["preExistingWorkspace"], false);
    assert_eq!(out["inTree"], false);
    assert!(out["files"].as_array().unwrap().len() >= 5);
    assert!(dir.join("gait.jsonc").is_file());
    assert!(dir.join(".git").exists());
    let again = gait(cwd.path(), &["init", "--gait-dir", s(&dir)]);
    assert_eq!(again.code, Some(0));
    assert_eq!(json(&again.stdout)["preExistingWorkspace"], true);
}

#[test]
fn init_without_gait_dir_is_a_usage_error() {
    let cwd = tempfile::tempdir().unwrap();
    let run = gait(cwd.path(), &["init"]);
    assert_eq!(run.code, Some(2), "{}", run.stderr);
    assert_eq!(error_code(&run), "op_registry.usage.missing");
    assert!(!cwd.path().join("gait.jsonc").exists(), "nothing is created in the cwd");
}

#[test]
fn a_positional_path_is_no_longer_accepted() {
    let cwd = tempfile::tempdir().unwrap();
    let dir = cwd.path().join("home");
    let run = gait(cwd.path(), &["init", s(&dir)]);
    assert_eq!(run.code, Some(2), "{}", run.stderr);
    assert!(!dir.exists());
}

#[test]
fn init_inside_a_git_repo_without_force_is_refused_with_65() {
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    let dir = repo.path().join("gait-home");
    let run = gait(repo.path(), &["init", "--gait-dir", s(&dir)]);
    assert_eq!(run.code, Some(65), "{}", run.stderr);
    assert!(run.stderr.contains("would live inside git repo"), "{}", run.stderr);
    let forced = gait(repo.path(), &["init", "--gait-dir", s(&dir), "--force"]);
    assert_eq!(forced.code, Some(0), "{}", forced.stderr);
    assert_eq!(json(&forced.stdout)["inTree"], true);
    assert!(forced.stderr.contains("--allow-intree"), "a notice warns: {}", forced.stderr);
}

#[test]
fn init_at_a_file_is_infrastructure_69() {
    let cwd = tempfile::tempdir().unwrap();
    let file = cwd.path().join("occupied");
    std::fs::write(&file, b"x").unwrap();
    let run = gait(cwd.path(), &["init", "--gait-dir", s(&file)]);
    assert_eq!(run.code, Some(69), "{}", run.stderr);
}

#[test]
fn link_writes_a_gaitref_that_later_commands_resolve() {
    let cwd = tempfile::tempdir().unwrap();
    let dir = cwd.path().join("home");
    assert_eq!(gait(cwd.path(), &["init", "--gait-dir", s(&dir)]).code, Some(0));
    let work = cwd.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let run = gait(&work, &["link", "--gait-dir", s(&dir)]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let out = json(&run.stdout);
    assert_eq!(out["target"], s(&dir));
    assert!(out["gaitref"].as_str().unwrap().ends_with(".gaitref"));
    assert_eq!(
        std::fs::read_to_string(work.join(".gaitref")).unwrap(),
        format!("gaitdir: {}\n", dir.display())
    );
    // The pointer is found by walk-up with no flag.
    let sessions = gait(&work, &["sessions"]);
    assert_eq!(sessions.code, Some(0), "{}", sessions.stderr);
    assert_eq!(json(&sessions.stdout), Value::Array(vec![]));
}

#[test]
fn link_to_a_missing_gait_dir_is_not_found_and_without_the_flag_a_usage_error() {
    let cwd = tempfile::tempdir().unwrap();
    let nowhere = cwd.path().join("nowhere");
    let run = gait(cwd.path(), &["link", "--gait-dir", s(&nowhere)]);
    assert_eq!(run.code, Some(65), "{}", run.stderr);
    assert_eq!(error_code(&run), "gait.op.not_found");
    assert!(run.stderr.contains("gait init --gait-dir"), "{}", run.stderr);
    assert!(!cwd.path().join(".gaitref").exists());
    let bare = gait(cwd.path(), &["link"]);
    assert_eq!(bare.code, Some(2), "{}", bare.stderr);
}

#[test]
fn link_where_the_pointer_cannot_be_written_is_69() {
    let cwd = tempfile::tempdir().unwrap();
    let dir = cwd.path().join("home");
    assert_eq!(gait(cwd.path(), &["init", "--gait-dir", s(&dir)]).code, Some(0));
    let work = cwd.path().join("work");
    std::fs::create_dir_all(work.join(".gaitref")).unwrap();
    let run = gait(&work, &["link", "--gait-dir", s(&dir)]);
    assert_eq!(run.code, Some(69), "{}", run.stderr);
}

#[test]
fn manual_install_writes_the_skill_and_refuses_to_overwrite_without_force() {
    let cwd = tempfile::tempdir().unwrap();
    let dir = cwd.path().join("home");
    assert_eq!(gait(cwd.path(), &["init", "--gait-dir", s(&dir)]).code, Some(0));
    let run = gait(cwd.path(), &["manual", "install", "--gait-dir", s(&dir)]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let path = dir.join("skills/gait-manual/SKILL.md");
    assert_eq!(json(&run.stdout)["installed"], s(&path));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("---\nname: gait-manual\n"), "{}", &text[..60]);
    let head = text.split("\n---\n").next().unwrap();
    assert!(head.contains("\nmanual-source-hash: "), "{head}");
    assert!(text.contains("### Config fields"));
    assert!(text.contains("### gait init"), "commands come from the registry");

    let again = gait(cwd.path(), &["manual", "install", "--gait-dir", s(&dir)]);
    assert_eq!(again.code, Some(65), "{}", again.stderr);
    assert!(again.stderr.contains("--force"), "{}", again.stderr);
    let forced = gait(cwd.path(), &["manual", "install", "--gait-dir", s(&dir), "--force"]);
    assert_eq!(forced.code, Some(0), "{}", forced.stderr);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "the manual is deterministic");
}

#[test]
fn manual_prints_markdown_and_json() {
    let cwd = tempfile::tempdir().unwrap();
    let md = gait(cwd.path(), &["manual"]);
    assert_eq!(md.code, Some(0), "{}", md.stderr);
    assert!(json(&md.stdout)["markdown"].as_str().unwrap().contains("# gait"));
    let descriptors = gait(cwd.path(), &["manual", "--json"]);
    assert_eq!(descriptors.code, Some(0), "{}", descriptors.stderr);
    let ops = json(&descriptors.stdout)["ops"].as_array().unwrap().len();
    assert!(ops > 30, "{ops}");
}

#[test]
fn the_old_cli_is_gone_from_the_library() {
    // The old clap tree is deleted: help is the registry's, listing the verbs.
    let cwd = tempfile::tempdir().unwrap();
    let help = gait(cwd.path(), &["--help"]);
    assert_eq!(help.code, Some(0));
    for verb in ["init", "link", "manual", "sessions", "auth", "workflow"] {
        assert!(help.stdout.contains(verb), "help lacks {verb}: {}", help.stdout);
    }
}

#[test]
fn workflow_keeps_its_clap_grammar() {
    let cwd = tempfile::tempdir().unwrap();
    let dir = cwd.path().join("home");
    assert_eq!(gait(cwd.path(), &["init", "--gait-dir", s(&dir)]).code, Some(0));
    let list = gait(cwd.path(), &["workflow", "list", "--gait-dir", s(&dir)]);
    assert_eq!(list.code, Some(0), "{}", list.stderr);
    let bad = gait(cwd.path(), &["workflow", "frobnicate", "--gait-dir", s(&dir)]);
    assert_eq!(bad.code, Some(65), "{}", bad.stderr);
}
