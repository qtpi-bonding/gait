//! Developer-declared evidence commands and run-level record persistence.

use gait_core::proto::gait::workflow::v1::{RecordCommand, Workflow};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Instant;
use walkdir::WalkDir;

use crate::convert::def_content_hash;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecordCommandResult {
    pub id: String,
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkflowRecord {
    pub run_id: String,
    pub workflow_hash: String,
    pub initial_tree_hash: String,
    pub final_tree_hash: Option<String>,
    pub workflow_status: String,
    pub commands: Vec<RecordCommandResult>,
}

pub fn record_path(run_dir: &Path) -> PathBuf {
    run_dir.join("record.json")
}

pub async fn begin(
    run_id: &str,
    workflow: &Workflow,
    worktree: &Path,
) -> anyhow::Result<WorkflowRecord> {
    let initial_tree_hash = tree_fingerprint(worktree)?;
    let mut commands = Vec::new();
    if let Some(config) = &workflow.record {
        let mut ids = std::collections::HashSet::new();
        for command in &config.commands {
            if command.id.is_empty() {
                anyhow::bail!("record command id must not be empty");
            }
            if command.command.trim().is_empty() {
                anyhow::bail!("record command '{}' must not be empty", command.id);
            }
            if command.timeout_ms == 0 {
                anyhow::bail!("record command '{}' timeout must be positive", command.id);
            }
            if !ids.insert(command.id.clone()) {
                anyhow::bail!("duplicate record command id '{}'", command.id);
            }
            commands.push(run_command(command, worktree).await);
        }
    }
    Ok(WorkflowRecord {
        run_id: run_id.to_string(),
        workflow_hash: def_content_hash(workflow),
        initial_tree_hash,
        final_tree_hash: None,
        workflow_status: "running".into(),
        commands,
    })
}

pub fn finish(
    record: &mut WorkflowRecord,
    path: &Path,
    workflow_status: &str,
    worktree: &Path,
) -> anyhow::Result<()> {
    record.workflow_status = workflow_status.to_string();
    record.final_tree_hash = Some(tree_fingerprint(worktree)?);
    write(record, path)
}

pub fn write(record: &WorkflowRecord, path: &Path) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub fn read(path: &Path) -> anyhow::Result<WorkflowRecord> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

async fn run_command(command: &RecordCommand, worktree: &Path) -> RecordCommandResult {
    let timeout_ms = command.timeout_ms;
    let start = Instant::now();
    let mut child = tokio::process::Command::new("sh");
    child
        .arg("-c")
        .arg(&command.command)
        .current_dir(worktree)
        .kill_on_drop(true);
    let result =
        tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), child.output()).await;
    let (exit_code, stdout, stderr, timed_out) = match result {
        Ok(Ok(output)) => (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            false,
        ),
        Ok(Err(error)) => (-1, String::new(), error.to_string(), false),
        Err(_) => (
            -1,
            String::new(),
            format!("record command timed out after {timeout_ms}ms"),
            true,
        ),
    };
    RecordCommandResult {
        id: command.id.clone(),
        command: command.command.clone(),
        exit_code,
        stdout,
        stderr,
        duration_ms: start.elapsed().as_millis() as u64,
        timed_out,
    }
}

/// Hash relevant worktree contents without inferring anything about its tools.
/// Ignored files and Git metadata are excluded; paths, type, mode, and bytes
/// are included in stable path order.
pub fn tree_fingerprint(worktree: &Path) -> anyhow::Result<String> {
    let repo = git2::Repository::open(worktree).ok();
    let mut entries = Vec::new();
    for entry in WalkDir::new(worktree).follow_links(false) {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(worktree)?;
        if relative.as_os_str().is_empty() || relative == Path::new(".git") {
            continue;
        }
        if relative
            .components()
            .next()
            .is_some_and(|c| c.as_os_str() == ".git")
        {
            continue;
        }
        if let Some(repo) = &repo {
            let rel = relative.to_string_lossy().replace('\\', "/");
            if repo.status_should_ignore(Path::new(&rel)).unwrap_or(false) {
                continue;
            }
        }
        let metadata = std::fs::symlink_metadata(path)?;
        let kind = if metadata.file_type().is_symlink() {
            "symlink"
        } else if metadata.is_dir() {
            "dir"
        } else {
            "file"
        };
        let mode = permissions_mode(&metadata);
        let bytes = if metadata.file_type().is_symlink() {
            std::fs::read_link(path)?
                .to_string_lossy()
                .as_bytes()
                .to_vec()
        } else if metadata.is_file() {
            std::fs::read(path)?
        } else {
            Vec::new()
        };
        entries.push((
            relative.to_string_lossy().replace('\\', "/"),
            kind.to_string(),
            mode,
            bytes,
        ));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (path, kind, mode, bytes) in entries {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(kind.as_bytes());
        hasher.update([0]);
        hasher.update(mode.to_le_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
        hasher.update([0xff]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(unix)]
fn permissions_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn permissions_mode(_metadata: &std::fs::Metadata) -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_core::proto::gait::workflow::v1::{RecordCommand, RecordConfig, Workflow};
    use tempfile::TempDir;

    #[tokio::test]
    async fn runs_only_declared_commands_and_captures_results() {
        let dir = TempDir::new().unwrap();
        let wf = Workflow {
            version: 1,
            record: Some(RecordConfig {
                commands: vec![RecordCommand {
                    id: "probe".into(),
                    command: "printf observed".into(),
                    timeout_ms: 5_000,
                }],
            }),
            ..Default::default()
        };
        let record = begin("run-1", &wf, dir.path()).await.unwrap();
        assert_eq!(record.commands.len(), 1);
        assert_eq!(record.commands[0].stdout, "observed");
        assert_eq!(record.commands[0].exit_code, 0);
    }

    #[tokio::test]
    async fn command_failure_is_captured_without_changing_workflow_status() {
        let dir = TempDir::new().unwrap();
        let wf = Workflow {
            record: Some(RecordConfig {
                commands: vec![RecordCommand {
                    id: "missing".into(),
                    command: "exit 7".into(),
                    timeout_ms: 5_000,
                }],
            }),
            ..Default::default()
        };
        let record = begin("run-1", &wf, dir.path()).await.unwrap();
        assert_eq!(record.commands[0].exit_code, 7);
        assert_eq!(record.workflow_status, "running");
    }

    #[test]
    fn fingerprint_changes_when_file_bytes_change() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let before = tree_fingerprint(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.txt"), "two").unwrap();
        let after = tree_fingerprint(dir.path()).unwrap();
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn timeout_is_recorded() {
        let dir = TempDir::new().unwrap();
        let wf = Workflow {
            record: Some(RecordConfig {
                commands: vec![RecordCommand {
                    id: "slow".into(),
                    command: "sleep 5".into(),
                    timeout_ms: 20,
                }],
            }),
            ..Default::default()
        };
        let record = begin("run-1", &wf, dir.path()).await.unwrap();
        assert!(record.commands[0].timed_out);
        assert_ne!(record.commands[0].exit_code, 0);
    }
}
