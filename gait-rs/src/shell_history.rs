use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ShellEntry {
    pub cmd: String,
    pub output: String,
    pub ts: u64,
}

pub fn append(path: &Path, cmd: &str, output: &str, ts: u64) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let entry = ShellEntry {
        cmd: cmd.to_string(),
        output: output.to_string(),
        ts,
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{}", serde_json::to_string(&entry)?)?;
    Ok(())
}

/// Recall the Nth most recent entry (n=1 = most recent).
/// Returns None if the file doesn't exist or n exceeds history length.
pub fn recall(path: &Path, n: usize) -> anyhow::Result<Option<ShellEntry>> {
    if !path.exists() {
        return Ok(None);
    }
    let f = std::fs::File::open(path)?;
    let reader = BufReader::new(f);
    let lines: Vec<String> = reader
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.is_empty())
        .collect();
    if n == 0 || n > lines.len() {
        return Ok(None);
    }
    let line = &lines[lines.len() - n];
    let entry: ShellEntry = serde_json::from_str(line)?;
    Ok(Some(entry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ts() -> u64 { 1_000_000 }

    #[test]
    fn recall_from_missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        assert!(recall(&path, 1).unwrap().is_none());
    }

    #[test]
    fn append_and_recall_single_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        append(&path, "ls -la", "total 8\nfoo", ts()).unwrap();
        let entry = recall(&path, 1).unwrap().unwrap();
        assert_eq!(entry.cmd, "ls -la");
        assert_eq!(entry.output, "total 8\nfoo");
    }

    #[test]
    fn recall_nth_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        append(&path, "cmd1", "out1", ts()).unwrap();
        append(&path, "cmd2", "out2", ts()).unwrap();
        append(&path, "cmd3", "out3", ts()).unwrap();
        // @-1 = most recent
        assert_eq!(recall(&path, 1).unwrap().unwrap().cmd, "cmd3");
        // @-2 = second most recent
        assert_eq!(recall(&path, 2).unwrap().unwrap().cmd, "cmd2");
        // @-3 = oldest
        assert_eq!(recall(&path, 3).unwrap().unwrap().cmd, "cmd1");
    }

    #[test]
    fn recall_beyond_history_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        append(&path, "cmd1", "out1", ts()).unwrap();
        assert!(recall(&path, 2).unwrap().is_none());
    }

    #[test]
    fn append_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/deep/shell.jsonl");
        append(&path, "pwd", "/home", ts()).unwrap();
        assert!(path.exists());
    }
}
