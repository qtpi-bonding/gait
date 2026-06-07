# gait-rs Phase 3: Session Layer + Turn Runner + CLI

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a working `gait` Rust binary — session git-worktree management via git2, append-only JSONL transcripts, an agentic turn loop that streams the LLM and dispatches tools with permission gating, and a `gait run` CLI command.

**Architecture:** Four new modules added to `gait-ai`: `git` (git2 ops — HEAD hash, dirty status, worktree create/commit), `session` (transcript event types + JSONL helpers + message reconstruction from events), `config`/`auth` (gait.jsonc + API key resolution), and `turn` (agentic streaming loop with per-tool-call event logging and permission gate). A `src/bin/gait.rs` binary wires them together. Session format is wire-compatible with the TypeScript gait (same JSONL schema, same `sessions/<id>/` git-worktree layout).

**Tech Stack:** `git2 0.19` (session worktrees, commits, branch ops — bundled libgit2, no system git required at runtime), `serde`/`serde_json` (JSONL events), `tokio` (async), existing `gait-ai` provider + `stream_text` infrastructure, `futures-util` (streaming), `uuid` (tool-call IDs)

---

## File Map

```
gait-rs/
  Cargo.toml                        ← add git2, uuid
  src/
    lib.rs                          ← add pub mod git, session, config, auth, turn
    git.rs                          ← git2 operations (no session logic)
    session/
      mod.rs                        ← pub use transcript::*; pub use context::*;
      transcript.rs                 ← AnyEvent enum (18 variants) + JSONL helpers
      context.rs                    ← reconstruct_messages(path) -> Vec<Message>
    config.rs                       ← GaitConfig, load_config(workspace)
    auth.rs                         ← resolve_api_key(provider) — env var + auth.json
    turn.rs                         ← SessionLock, TurnOpts, run_turn()
    bin/
      gait.rs                       ← CLI: gait run [--session] [--task] [--auto] ...
  tests/
    integration_turn.rs             ← end-to-end turn test with MockProvider
```

---

## Task 1: git2 dependency + `src/git.rs`

**Files:**
- Modify: `gait-rs/Cargo.toml`
- Create: `gait-rs/src/git.rs`

- [x] **Step 1: Add git2 and uuid to Cargo.toml**

In `gait-rs/Cargo.toml`, add to `[dependencies]`:

```toml
git2 = "0.19"
uuid = { version = "1", features = ["v4"] }
```

- [x] **Step 2: Write failing tests for git operations**

Create `gait-rs/src/git.rs` with just the function signatures and a `#[cfg(test)]` block:

```rust
use std::path::{Path, PathBuf};

pub fn get_codebase_hash(codebase: &Path) -> String { todo!() }
pub fn get_dirty_files(codebase: &Path) -> Vec<String> { todo!() }
pub fn create_session_worktree(workspace: &Path, session_id: &str) -> anyhow::Result<()> { todo!() }
pub fn commit_session(workspace: &Path, session_id: &str, message: &str) -> anyhow::Result<()> { todo!() }
pub fn list_active_sessions(workspace: &Path) -> anyhow::Result<Vec<String>> { todo!() }

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repo(dir: &Path) -> git2::Repository {
        let repo = git2::Repository::init(dir).unwrap();
        // git2 needs user config for commits; set locally
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        // Create initial commit on main so branch ops work
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        // Rename branch to main
        repo.find_branch("master", git2::BranchType::Local)
            .or_else(|_| repo.find_branch("main", git2::BranchType::Local))
            .and_then(|mut b| { b.rename("main", false)?; Ok(()) })
            .ok();
        repo
    }

    #[test]
    fn get_codebase_hash_returns_hex() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        let hash = get_codebase_hash(dir.path());
        assert_eq!(hash.len(), 40, "expected 40-char SHA1, got: {hash}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "not hex: {hash}");
    }

    #[test]
    fn get_dirty_files_empty_on_clean_repo() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        assert!(get_dirty_files(dir.path()).is_empty());
    }

    #[test]
    fn get_dirty_files_lists_untracked() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("new.txt"), "hello").unwrap();
        let dirty = get_dirty_files(dir.path());
        assert!(!dirty.is_empty(), "expected dirty files");
    }

    #[test]
    fn create_session_worktree_creates_dir_and_branch() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        create_session_worktree(ws.path(), "sabc1234").unwrap();
        assert!(ws.path().join("sessions/sabc1234").exists());
        assert!(ws.path().join("sessions/sabc1234/.git").exists());
    }

    #[test]
    fn create_session_worktree_idempotent() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        create_session_worktree(ws.path(), "sabc1234").unwrap();
        create_session_worktree(ws.path(), "sabc1234").unwrap(); // no error
    }

    #[test]
    fn commit_session_creates_commit() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        create_session_worktree(ws.path(), "sabc1234").unwrap();
        let tx = ws.path().join("sessions/sabc1234/transcript.jsonl");
        std::fs::write(&tx, "{\"type\":\"session_start\"}\n").unwrap();
        commit_session(ws.path(), "sabc1234", "session(sabc1234): turn 1").unwrap();
        // Verify: repo in session dir has 2+ commits (init + our commit)
        let repo = git2::Repository::open(ws.path().join("sessions/sabc1234")).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.message().unwrap(), "session(sabc1234): turn 1");
    }

    #[test]
    fn list_active_sessions_returns_unmerged() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        create_session_worktree(ws.path(), "sabc1234").unwrap();
        let sessions = list_active_sessions(ws.path()).unwrap();
        assert!(sessions.contains(&"sabc1234".to_string()), "got: {:?}", sessions);
    }
}
```

- [x] **Step 3: Run tests to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test git::tests 2>&1 | head -30
```

Expected: compilation errors or `todo!()` panics — confirms tests are wired.

- [x] **Step 4: Implement `src/git.rs`**

Replace the stub functions with:

```rust
use std::path::{Path, PathBuf};
use git2::{Repository, BranchType, IndexAddOption, Signature};

pub fn get_codebase_hash(codebase: &Path) -> String {
    Repository::open(codebase)
        .and_then(|r| r.head())
        .and_then(|h| h.peel_to_commit().map(|c| c.id().to_string()))
        .unwrap_or_else(|_| "none".to_string())
}

pub fn get_dirty_files(codebase: &Path) -> Vec<String> {
    let Ok(repo) = Repository::open(codebase) else { return vec![] };
    let Ok(statuses) = repo.statuses(None) else { return vec![] };
    statuses.iter()
        .filter_map(|e| {
            let s = e.status();
            if s.is_empty() || s == git2::Status::CURRENT { return None; }
            e.path().map(|p| p.to_string())
        })
        .collect()
}

pub fn create_session_worktree(workspace: &Path, session_id: &str) -> anyhow::Result<()> {
    let session_dir = workspace.join("sessions").join(session_id);
    // Already a worktree — idempotent
    if session_dir.join(".git").exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&session_dir)?;
    let repo = Repository::open(workspace)?;

    let branch_name = format!("session/{}", session_id);

    // Re-attach if branch exists but dir was cleaned
    if let Ok(branch) = repo.find_branch(&branch_name, BranchType::Local) {
        let reference = branch.into_reference();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        repo.worktree(session_id, &session_dir, Some(&opts))?;
        return Ok(());
    }

    // Fresh: create branch from HEAD, add worktree
    let head_commit = repo.head()?.peel_to_commit()?;
    let branch = repo.branch(&branch_name, &head_commit, false)?;
    let reference = branch.into_reference();
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(&reference));
    repo.worktree(session_id, &session_dir, Some(&opts))?;

    // Initial commit: .gitignore with "lock"
    let wt_repo = Repository::open(&session_dir)?;
    std::fs::write(session_dir.join(".gitignore"), "lock\n")?;
    let sig = wt_repo.signature()
        .or_else(|_| Signature::now("gait", "gait@localhost"))?;
    let mut index = wt_repo.index()?;
    index.add_path(Path::new(".gitignore"))?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = wt_repo.find_tree(tree_id)?;
    let parent = wt_repo.head()?.peel_to_commit()?;
    wt_repo.commit(Some("HEAD"), &sig, &sig,
        &format!("session({}): init", session_id), &tree, &[&parent])?;

    Ok(())
}

pub fn commit_session(workspace: &Path, session_id: &str, message: &str) -> anyhow::Result<()> {
    let session_dir = workspace.join("sessions").join(session_id);
    let repo = Repository::open(&session_dir)?;
    let sig = repo.signature()
        .or_else(|_| Signature::now("gait", "gait@localhost"))?;
    let mut index = repo.index()?;
    index.add_all(["."].iter(), IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    // Nothing to commit if tree matches HEAD
    let head_commit = repo.head()?.peel_to_commit()?;
    if head_commit.tree_id() == tree_id {
        return Ok(());
    }
    let tree = repo.find_tree(tree_id)?;
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&head_commit])?;
    Ok(())
}

pub fn list_active_sessions(workspace: &Path) -> anyhow::Result<Vec<String>> {
    let repo = Repository::open(workspace)?;
    let main_oid = repo.find_branch("main", BranchType::Local)
        .or_else(|_| repo.find_branch("master", BranchType::Local))?
        .get().peel_to_commit()?.id();
    let mut sessions = vec![];
    for item in repo.branches(Some(BranchType::Local))? {
        let (branch, _) = item?;
        let name = branch.name()?.unwrap_or("").to_string();
        let Some(id) = name.strip_prefix("session/") else { continue };
        let branch_oid = branch.get().peel_to_commit()?.id();
        // Not merged = main is NOT a descendant of branch
        let is_merged = repo.graph_descendant_of(main_oid, branch_oid).unwrap_or(false);
        if !is_merged {
            sessions.push(id.to_string());
        }
    }
    Ok(sessions)
}
```

- [x] **Step 5: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test git::tests 2>&1
```

Expected: all 7 git tests pass.

- [x] **Step 6: Commit**

```bash
git add gait-rs/Cargo.toml gait-rs/src/git.rs
git commit -m "feat(rs): git2 session operations — worktree create/commit/list"
```

---

## Task 2: `src/session/transcript.rs`

**Files:**
- Create: `gait-rs/src/session/mod.rs`
- Create: `gait-rs/src/session/transcript.rs`

All 18 event variants matching the TypeScript JSONL schema exactly. Supporting structs. JSONL helpers.

- [x] **Step 1: Create `src/session/mod.rs`**

```rust
pub mod transcript;
pub mod context;
pub use transcript::*;
pub use context::reconstruct_messages;
```

- [x] **Step 2: Write failing tests in `src/session/transcript.rs`**

Create the file with test stubs only first:

```rust
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ── Supporting types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserTurnContext {
    pub codebase_hash: Option<String>,
    pub codebase_dirty: Option<bool>,
    pub dirty_files: Option<Vec<String>>,
    pub injected_files: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnOutput {
    pub summary: String,
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionUsage {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClarifyItem {
    pub id: String,
    pub content: String,
    pub why: String,
    pub kind: String, // "is" | "ought"
    pub links: Option<Vec<LinkRef>>,
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClarifyReplyItem {
    pub id: String,
    pub content: String,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClarifyConvergenceItem {
    pub id: String,
    pub proposal: String,
    pub response: Option<String>,
    pub consistent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InformItem {
    pub id: String,
    pub content: String,
    pub context: Option<String>,
    pub links: Option<Vec<LinkRef>>,
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InformReplyItem {
    pub id: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InformConvergenceItem {
    pub id: String,
    pub proposal: String,
    pub response: Option<String>,
    pub consistent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkRef {
    pub target: String,
    pub relation: String, // "depends_on" | "related_to" | "part_of"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionItem {
    pub id: String,
    pub cmd: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsReplyItem {
    pub id: String,
    pub decision: String, // "approve" | "deny" | "instruct"
    pub text: Option<String>,
}

// ── Main event enum ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnyEvent {
    SessionStart {
        schema_version: u32,
        id: String,
        model: String,
        codebase_hash: String,
        codebase_dirty: bool,
        ts: u64,
    },
    UserTurn {
        content: String,
        context: Option<UserTurnContext>,
        ts: u64,
    },
    AssistantText {
        content: String,
        ts: u64,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
        ts: u64,
    },
    ToolResult {
        id: String,
        output: String,
        ts: u64,
    },
    CommandResult {
        call_id: String,
        cmd: String,
        output: String,
        exit_code: i32,
        duration_ms: u64,
        timed_out: bool,
        ts: u64,
    },
    Error {
        message: String,
        phase: String, // "llm" | "tool" | "git" | "init"
        ts: u64,
    },
    Clarify {
        items: Vec<ClarifyItem>,
        ts: u64,
    },
    ClarifyReply {
        items: Vec<ClarifyReplyItem>,
        ts: u64,
    },
    ClarifyConvergence {
        items: Vec<ClarifyConvergenceItem>,
        ts: u64,
    },
    Inform {
        items: Vec<InformItem>,
        ts: u64,
    },
    InformReply {
        items: Vec<InformReplyItem>,
        ts: u64,
    },
    InformConvergence {
        items: Vec<InformConvergenceItem>,
        ts: u64,
    },
    AgentUpdate {
        session_id: String,
        status: String, // "done" | "clarify" | "inform"
        summary: String,
        items: Option<serde_json::Value>,
        ts: u64,
    },
    Permissions {
        items: Vec<PermissionItem>,
        ts: u64,
    },
    PermissionsReply {
        from: String,
        items: Vec<PermissionsReplyItem>,
        ts: u64,
    },
    TurnComplete {
        usage: TurnUsage,
        codebase_hash: String,
        codebase_dirty: bool,
        output: TurnOutput,
        ts: u64,
    },
    Compaction {
        summary: String,
        turns_covered: u32,
        from_ts: u64,
        to_ts: u64,
        usage: CompactionUsage,
        ts: u64,
    },
}

// ── JSONL helpers ─────────────────────────────────────────────────────────────

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn append_event(path: &Path, event: &AnyEvent) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", serde_json::to_string(event)?)?;
    Ok(())
}

pub fn read_events(path: &Path) -> anyhow::Result<Vec<AnyEvent>> {
    if !path.exists() { return Ok(vec![]); }
    let text = std::fs::read_to_string(path)?;
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<AnyEvent>(l).map_err(anyhow::Error::from))
        .collect()
}

pub fn last_event(path: &Path) -> anyhow::Result<Option<AnyEvent>> {
    Ok(read_events(path)?.into_iter().last())
}

pub fn count_turn_complete(events: &[AnyEvent]) -> usize {
    events.iter().filter(|e| matches!(e, AnyEvent::TurnComplete { .. })).count()
}

pub fn last_compaction_index(events: &[AnyEvent]) -> Option<usize> {
    events.iter().rposition(|e| matches!(e, AnyEvent::Compaction { .. }))
}

pub struct InitSessionOpts {
    pub transcript_path: PathBuf,
    pub id: String,
    pub model: String,
    pub codebase_hash: String,
    pub codebase_dirty: bool,
}

pub fn init_session(opts: InitSessionOpts) -> anyhow::Result<()> {
    append_event(&opts.transcript_path, &AnyEvent::SessionStart {
        schema_version: 2,
        id: opts.id,
        model: opts.model,
        codebase_hash: opts.codebase_hash,
        codebase_dirty: opts.codebase_dirty,
        ts: now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn append_and_read_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        append_event(&path, &AnyEvent::UserTurn {
            content: "hello".into(),
            context: None,
            ts: 1000,
        }).unwrap();
        append_event(&path, &AnyEvent::AssistantText {
            content: "world".into(),
            ts: 2000,
        }).unwrap();
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AnyEvent::UserTurn { content, .. } if content == "hello"));
        assert!(matches!(&events[1], AnyEvent::AssistantText { content, .. } if content == "world"));
    }

    #[test]
    fn init_session_writes_session_start() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        init_session(InitSessionOpts {
            transcript_path: path.clone(),
            id: "s1234".into(),
            model: "anthropic/claude-sonnet-4-6".into(),
            codebase_hash: "abc123".into(),
            codebase_dirty: false,
        }).unwrap();
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AnyEvent::SessionStart { id, .. } if id == "s1234"));
    }

    #[test]
    fn count_turn_complete_counts_correctly() {
        let events = vec![
            AnyEvent::UserTurn { content: "x".into(), context: None, ts: 0 },
            AnyEvent::TurnComplete {
                usage: TurnUsage { input: 1, output: 1, cache_read: 0, cache_write: 0 },
                codebase_hash: "a".into(),
                codebase_dirty: false,
                output: TurnOutput { summary: "done".into(), result: None },
                ts: 1,
            },
            AnyEvent::UserTurn { content: "y".into(), context: None, ts: 2 },
            AnyEvent::TurnComplete {
                usage: TurnUsage { input: 1, output: 1, cache_read: 0, cache_write: 0 },
                codebase_hash: "a".into(),
                codebase_dirty: false,
                output: TurnOutput { summary: "done".into(), result: None },
                ts: 3,
            },
        ];
        assert_eq!(count_turn_complete(&events), 2);
    }

    #[test]
    fn last_compaction_index_finds_last() {
        let events = vec![
            AnyEvent::UserTurn { content: "x".into(), context: None, ts: 0 },
            AnyEvent::Compaction {
                summary: "s1".into(), turns_covered: 1,
                from_ts: 0, to_ts: 1,
                usage: CompactionUsage { input: 10, output: 5 }, ts: 1,
            },
            AnyEvent::UserTurn { content: "y".into(), context: None, ts: 2 },
        ];
        assert_eq!(last_compaction_index(&events), Some(1));
    }

    #[test]
    fn all_event_variants_serialize_roundtrip() {
        let events: Vec<AnyEvent> = vec![
            AnyEvent::SessionStart { schema_version: 2, id: "s1".into(), model: "m".into(), codebase_hash: "h".into(), codebase_dirty: false, ts: 0 },
            AnyEvent::UserTurn { content: "t".into(), context: None, ts: 0 },
            AnyEvent::AssistantText { content: "a".into(), ts: 0 },
            AnyEvent::ToolCall { id: "i".into(), name: "read".into(), input: serde_json::json!({}), ts: 0 },
            AnyEvent::ToolResult { id: "i".into(), output: "r".into(), ts: 0 },
            AnyEvent::Error { message: "e".into(), phase: "llm".into(), ts: 0 },
            AnyEvent::Permissions { items: vec![], ts: 0 },
            AnyEvent::PermissionsReply { from: "user".into(), items: vec![], ts: 0 },
            AnyEvent::TurnComplete { usage: TurnUsage { input:1,output:1,cache_read:0,cache_write:0 }, codebase_hash:"h".into(), codebase_dirty:false, output: TurnOutput{summary:"s".into(),result:None}, ts:0 },
        ];
        for e in &events {
            let json = serde_json::to_string(e).unwrap();
            let back: AnyEvent = serde_json::from_str(&json).unwrap();
            // Verify type tag survived
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(v.get("type").is_some(), "missing type tag in: {json}");
        }
    }
}
```

- [x] **Step 3: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test session::transcript::tests 2>&1
```

Expected: all 5 tests pass.

- [x] **Step 4: Commit**

```bash
git add gait-rs/src/session/
git commit -m "feat(rs): session transcript — 18 event variants + JSONL helpers"
```

---

## Task 3: `src/session/context.rs`

**Files:**
- Create: `gait-rs/src/session/context.rs`

Port of `session/context.ts` — turns JSONL events into `Vec<Message>` for the LLM.

- [x] **Step 1: Write failing tests**

Create `gait-rs/src/session/context.rs`:

```rust
use std::path::Path;
use crate::types::{Message, MessageContent, AssistantContent, AssistantPart, ToolCall, ToolResult, ToolResultKind};
use crate::session::transcript::{AnyEvent, read_events, last_compaction_index};

pub fn reconstruct_messages(path: &Path) -> anyhow::Result<Vec<Message>> {
    let all_events = read_events(path)?;
    if all_events.is_empty() { return Ok(vec![]); }

    // Start from last compaction breakpoint (or beginning)
    let start = last_compaction_index(&all_events).unwrap_or(0);
    // If there IS a compaction at `start`, include it (it provides the summary message)
    let events = &all_events[start..];

    let mut messages: Vec<Message> = vec![];
    let mut i = 0;

    while i < events.len() {
        match &events[i] {
            AnyEvent::Compaction { summary, .. } => {
                messages.push(Message::User {
                    content: MessageContent::Text(
                        format!("[Session history summary: {}]", summary)
                    ),
                });
                i += 1;
            }
            AnyEvent::SessionStart { .. } => { i += 1; }

            AnyEvent::AgentUpdate { session_id, status, summary, items, .. } => {
                let mut content = format!("[agent:{}] status: {}\nsummary: {}", session_id, status, summary);
                if let Some(items) = items {
                    content.push_str(&format!("\nitems: {}", items));
                }
                messages.push(Message::User { content: MessageContent::Text(content) });
                i += 1;
            }

            AnyEvent::UserTurn { content, .. } => {
                messages.push(Message::User { content: MessageContent::Text(content.clone()) });
                i += 1;
            }

            AnyEvent::AssistantText { content, .. } => {
                messages.push(Message::Assistant { content: AssistantContent::Text(content.clone()) });
                i += 1;
            }

            AnyEvent::ToolCall { .. } => {
                // Gather all consecutive tool_call events
                let mut calls: Vec<(String, String, serde_json::Value)> = vec![];
                while i < events.len() {
                    if let AnyEvent::ToolCall { id, name, input, .. } = &events[i] {
                        calls.push((id.clone(), name.clone(), input.clone()));
                        i += 1;
                    } else { break; }
                }
                // Gather consecutive tool_result events
                let mut results: Vec<(String, String)> = vec![];
                while i < events.len() {
                    if let AnyEvent::ToolResult { id, output, .. } = &events[i] {
                        results.push((id.clone(), output.clone()));
                        i += 1;
                    } else { break; }
                }

                let parts: Vec<AssistantPart> = calls.iter().map(|(id, name, args)| {
                    AssistantPart::ToolCall(ToolCall {
                        tool_call_id: id.clone(),
                        tool_name: name.clone(),
                        args: args.clone(),
                    })
                }).collect();
                messages.push(Message::Assistant { content: AssistantContent::Parts(parts) });

                if !results.is_empty() {
                    let tool_content: Vec<ToolResult> = results.iter().map(|(id, output)| {
                        ToolResult {
                            kind: ToolResultKind::ToolResult,
                            tool_call_id: id.clone(),
                            result: serde_json::Value::String(output.clone()),
                        }
                    }).collect();
                    messages.push(Message::Tool { content: tool_content });
                }
            }

            AnyEvent::ToolResult { .. } => { i += 1; } // orphan — skip

            AnyEvent::Clarify { items, .. } => {
                let text = items.iter().map(|q| {
                    format!("clarify ({}): {}\nwhy: {}", q.kind, q.content, q.why)
                }).collect::<Vec<_>>().join("\n\n");
                messages.push(Message::Assistant { content: AssistantContent::Text(text) });
                i += 1;
            }

            AnyEvent::ClarifyReply { items, .. } => {
                let text = items.iter().map(|r| {
                    let base = format!("{}: {}", r.id, r.content);
                    if let Some(reason) = &r.reasoning { format!("{}\nreason: {}", base, reason) }
                    else { base }
                }).collect::<Vec<_>>().join("\n");
                messages.push(Message::User { content: MessageContent::Text(text) });
                i += 1;
            }

            AnyEvent::ClarifyConvergence { items, .. } => {
                for item in items {
                    messages.push(Message::Assistant { content: AssistantContent::Text(format!("[proposal] {}", item.proposal)) });
                    if let Some(resp) = &item.response {
                        messages.push(Message::User { content: MessageContent::Text(format!("[response] {}", resp)) });
                    }
                }
                i += 1;
            }

            AnyEvent::Inform { items, .. } => {
                let text = items.iter().map(|it| {
                    let mut s = it.content.clone();
                    if let Some(ctx) = &it.context { s.push_str(&format!("\ncontext: {}", ctx)); }
                    s
                }).collect::<Vec<_>>().join("\n\n");
                messages.push(Message::Assistant { content: AssistantContent::Text(text) });
                i += 1;
            }

            AnyEvent::InformReply { items, .. } => {
                let text = items.iter().map(|r| format!("{}: {}", r.id, r.content)).collect::<Vec<_>>().join("\n");
                messages.push(Message::User { content: MessageContent::Text(text) });
                i += 1;
            }

            AnyEvent::Permissions { items, .. } => {
                let text = items.iter().map(|p| format!("permission requested: {} — {}", p.cmd, p.reason)).collect::<Vec<_>>().join("\n");
                messages.push(Message::Assistant { content: AssistantContent::Text(text) });
                i += 1;
            }

            AnyEvent::PermissionsReply { from, items, .. } => {
                let text = items.iter().map(|r| format!("{}: {}", r.id, r.decision)).collect::<Vec<_>>().join("\n");
                messages.push(Message::User { content: MessageContent::Text(format!("permissions reply (from: {})\n{}", from, text)) });
                i += 1;
            }

            AnyEvent::Error { message, phase, .. } => {
                messages.push(Message::User { content: MessageContent::Text(format!("error during {}: {}", phase, message)) });
                i += 1;
            }

            // Skip: TurnComplete, CommandResult, InformConvergence (not needed for LLM context)
            _ => { i += 1; }
        }
    }

    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::transcript::*;
    use tempfile::TempDir;

    fn write_events(dir: &std::path::Path, events: &[AnyEvent]) -> std::path::PathBuf {
        let path = dir.join("transcript.jsonl");
        for e in events { append_event(&path, e).unwrap(); }
        path
    }

    #[test]
    fn empty_transcript_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let msgs = reconstruct_messages(&path).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn user_and_assistant_text_reconstructed() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), &[
            AnyEvent::SessionStart { schema_version: 2, id: "s1".into(), model: "m".into(), codebase_hash: "h".into(), codebase_dirty: false, ts: 0 },
            AnyEvent::UserTurn { content: "hello".into(), context: None, ts: 1 },
            AnyEvent::AssistantText { content: "hi there".into(), ts: 2 },
        ]);
        let msgs = reconstruct_messages(&path).unwrap();
        assert_eq!(msgs.len(), 2); // session_start is skipped
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t == "hello"));
        assert!(matches!(&msgs[1], Message::Assistant { content: AssistantContent::Text(t) } if t == "hi there"));
    }

    #[test]
    fn tool_call_and_result_paired() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), &[
            AnyEvent::ToolCall { id: "id1".into(), name: "read".into(), input: serde_json::json!({"file_path":"a.rs"}), ts: 0 },
            AnyEvent::ToolResult { id: "id1".into(), output: "contents".into(), ts: 1 },
        ]);
        let msgs = reconstruct_messages(&path).unwrap();
        assert_eq!(msgs.len(), 2);
        if let Message::Assistant { content: AssistantContent::Parts(parts) } = &msgs[0] {
            assert_eq!(parts.len(), 1);
            assert!(matches!(&parts[0], AssistantPart::ToolCall(tc) if tc.tool_call_id == "id1"));
        } else { panic!("expected assistant parts"); }
        assert!(matches!(&msgs[1], Message::Tool { .. }));
    }

    #[test]
    fn parallel_tool_calls_grouped() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), &[
            AnyEvent::ToolCall { id: "id1".into(), name: "read".into(), input: serde_json::json!({}), ts: 0 },
            AnyEvent::ToolCall { id: "id2".into(), name: "ls".into(), input: serde_json::json!({}), ts: 1 },
            AnyEvent::ToolResult { id: "id1".into(), output: "r1".into(), ts: 2 },
            AnyEvent::ToolResult { id: "id2".into(), output: "r2".into(), ts: 3 },
        ]);
        let msgs = reconstruct_messages(&path).unwrap();
        assert_eq!(msgs.len(), 2);
        if let Message::Assistant { content: AssistantContent::Parts(parts) } = &msgs[0] {
            assert_eq!(parts.len(), 2);
        } else { panic!("expected 2 tool calls in one assistant message"); }
        if let Message::Tool { content } = &msgs[1] {
            assert_eq!(content.len(), 2);
        } else { panic!("expected tool message with 2 results"); }
    }

    #[test]
    fn compaction_breakpoint_discards_earlier_events() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), &[
            AnyEvent::UserTurn { content: "old".into(), context: None, ts: 0 },
            AnyEvent::AssistantText { content: "old response".into(), ts: 1 },
            AnyEvent::TurnComplete { usage: TurnUsage{input:1,output:1,cache_read:0,cache_write:0}, codebase_hash:"h".into(), codebase_dirty:false, output: TurnOutput{summary:"s".into(),result:None}, ts:2 },
            AnyEvent::Compaction { summary: "summary of old stuff".into(), turns_covered: 1, from_ts: 0, to_ts: 2, usage: CompactionUsage{input:10,output:5}, ts: 3 },
            AnyEvent::UserTurn { content: "new".into(), context: None, ts: 4 },
        ]);
        let msgs = reconstruct_messages(&path).unwrap();
        // Should have: compaction summary + "new" user turn
        assert_eq!(msgs.len(), 2, "got: {:?}", msgs.len());
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t.contains("summary of old stuff")));
        assert!(matches!(&msgs[1], Message::User { content: MessageContent::Text(t) } if t == "new"));
    }
}
```

- [x] **Step 2: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test session::context::tests 2>&1
```

Expected: all 5 tests pass.

- [x] **Step 3: Commit**

```bash
git add gait-rs/src/session/context.rs
git commit -m "feat(rs): session context — reconstruct messages from JSONL events"
```

---

## Task 4: `src/config.rs` + `src/auth.rs`

**Files:**
- Create: `gait-rs/src/config.rs`
- Create: `gait-rs/src/auth.rs`

Minimal config parser (strips `//` comments from JSONC) and API key resolution (env var first, then `~/.local/share/gait/auth.json`).

- [x] **Step 1: Write `src/config.rs`**

```rust
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigTool {
    pub name: String,
    pub description: String,
    pub cmd: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
}

fn default_cwd() -> String { "codebase".into() }

#[derive(Debug, Clone, Deserialize)]
pub struct GaitConfig {
    pub model: String,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub tools: Vec<ConfigTool>,
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f64,
}

fn default_compaction_threshold() -> f64 { 0.8 }

impl Default for GaitConfig {
    fn default() -> Self {
        Self {
            model: "anthropic/claude-sonnet-4-6".into(),
            allow: vec!["read".into(), "glob".into(), "grep".into(), "ls".into()],
            tools: vec![],
            compaction_threshold: 0.8,
        }
    }
}

fn strip_jsonc_comments(input: &str) -> String {
    input.lines().map(|line| {
        // Find // that isn't inside a string (count unescaped " before pos)
        let mut in_string = false;
        let mut prev_char = '\0';
        let chars: Vec<char> = line.chars().collect();
        for i in 0..chars.len() {
            if chars[i] == '"' && prev_char != '\\' { in_string = !in_string; }
            if !in_string && i + 1 < chars.len() && chars[i] == '/' && chars[i+1] == '/' {
                return line[..i].trim_end().to_string();
            }
            prev_char = chars[i];
        }
        line.to_string()
    }).collect::<Vec<_>>().join("\n")
}

pub fn load_config(workspace: &Path) -> GaitConfig {
    let path = workspace.join("gait.jsonc");
    if !path.exists() { return GaitConfig::default(); }
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let stripped = strip_jsonc_comments(&raw);
    serde_json::from_str(&stripped).unwrap_or_else(|e| {
        eprintln!("[gait] warning: could not parse gait.jsonc: {e}");
        GaitConfig::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn loads_default_when_no_file() {
        let dir = TempDir::new().unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.model, "anthropic/claude-sonnet-4-6");
    }

    #[test]
    fn parses_model_and_allow() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"
{
  "model": "minimax/MiniMax-M1",
  "allow": ["read", "write", "shell"]
}
"#).unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.model, "minimax/MiniMax-M1");
        assert_eq!(cfg.allow, vec!["read", "write", "shell"]);
    }

    #[test]
    fn strips_line_comments() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"
{
  // this is the model
  "model": "anthropic/claude-sonnet-4-6",
  "allow": ["read"] // read is safe
}
"#).unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.model, "anthropic/claude-sonnet-4-6");
    }
}
```

- [x] **Step 2: Write `src/auth.rs`**

```rust
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthEntry {
    Api { key: String },
    OAuth { refresh: String, access: String, expires: u64 },
    Wellknown { key: String, token: String },
}

/// Resolve API key for a provider.
/// Priority: environment variable > ~/.local/share/gait/auth.json
pub fn resolve_api_key(provider: &str) -> Option<String> {
    // 1. Check env var (ANTHROPIC_API_KEY, MINIMAX_API_KEY, OPENAI_API_KEY, etc.)
    let env_var = format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"));
    if let Ok(key) = std::env::var(&env_var) {
        if !key.is_empty() { return Some(key); }
    }
    // 2. Check auth.json
    let auth_path = dirs_next::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
        .join("gait")
        .join("auth.json");
    if !auth_path.exists() { return None; }
    let Ok(text) = std::fs::read_to_string(&auth_path) else { return None; };
    let Ok(map) = serde_json::from_str::<HashMap<String, AuthEntry>>(&text) else { return None; };
    match map.get(provider)? {
        AuthEntry::Api { key } => Some(key.clone()),
        AuthEntry::Wellknown { key, .. } => Some(key.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_none_for_unknown_provider() {
        // Should not panic even when provider doesn't exist
        let result = resolve_api_key("nonexistent_provider_xyzzy");
        // We can't assert None because the user might have this env var set,
        // but we can assert it doesn't panic
        let _ = result;
    }
}
```

- [x] **Step 3: Add `dirs-next` to Cargo.toml**

In `gait-rs/Cargo.toml` `[dependencies]`:

```toml
dirs-next = "2"
```

- [x] **Step 4: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test config::tests 2>&1
```

Expected: 3 config tests pass.

- [x] **Step 5: Commit**

```bash
git add gait-rs/Cargo.toml gait-rs/src/config.rs gait-rs/src/auth.rs
git commit -m "feat(rs): config parser (JSONC) + API key resolution"
```

---

## Task 5: `src/turn.rs`

**Files:**
- Create: `gait-rs/src/turn.rs`

The core agentic loop. Streams the provider, dispatches tools with permission gating, appends events to the transcript, commits after each turn.

- [x] **Step 1: Write `src/turn.rs`**

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;
use futures_util::StreamExt;
use crate::provider::{Provider, StreamChunk};
use crate::types::{Message, MessageContent, AssistantContent, AssistantPart, ToolCall, ToolResult, ToolResultKind, Usage};
use crate::types::Tool;
use crate::session::transcript::{
    AnyEvent, append_event, read_events, init_session, count_turn_complete, now_ms,
    InitSessionOpts, PermissionItem, PermissionsReplyItem, TurnUsage, TurnOutput,
};
use crate::session::context::reconstruct_messages;
use crate::git;

pub struct TurnOpts {
    pub session_id: String,
    pub task: Option<String>,
    pub auto: bool,
    pub workspace: PathBuf,
    pub codebase_path: PathBuf,
    pub allow: Vec<String>,
    pub system_prompt: String,
    pub model: String,  // stored in session_start event for wire-compat with TS gait
}

// ── Session lock (RAII PID file) ─────────────────────────────────────────────

pub struct SessionLock {
    path: PathBuf,
}

impl SessionLock {
    pub fn acquire(session_dir: &Path) -> anyhow::Result<Self> {
        let path = session_dir.join("lock");
        if path.exists() {
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            if let Ok(pid) = raw.trim().parse::<u32>() {
                // Check if that PID is still alive
                if std::path::Path::new(&format!("/proc/{}", pid)).exists()
                    || unsafe { libc::kill(pid as i32, 0) == 0 }
                {
                    anyhow::bail!("session {} is already running (PID {})", session_dir.display(), pid);
                }
            }
        }
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(Self { path })
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn build_turn_context(codebase_hash: &str, dirty_files: &[String]) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let mut ctx = format!("Current time (unix): {}\nCodebase commit: {}\n", secs, codebase_hash);
    if !dirty_files.is_empty() {
        ctx.push_str("Dirty files:\n");
        for f in dirty_files { ctx.push_str(&format!("  {}\n", f)); }
    }
    ctx
}

pub async fn run_turn(
    opts: TurnOpts,
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
) -> anyhow::Result<()> {
    git::create_session_worktree(&opts.workspace, &opts.session_id)?;

    let session_dir = opts.workspace.join("sessions").join(&opts.session_id);
    let tx_path = session_dir.join("transcript.jsonl");

    let _lock = SessionLock::acquire(&session_dir)?;

    let codebase_hash = git::get_codebase_hash(&opts.codebase_path);
    let dirty_files = git::get_dirty_files(&opts.codebase_path);
    let codebase_dirty = !dirty_files.is_empty();

    // Init transcript on first turn
    let existing = std::fs::read_to_string(&tx_path).unwrap_or_default();
    if existing.trim().is_empty() {
        init_session(InitSessionOpts {
            transcript_path: tx_path.clone(),
            id: opts.session_id.clone(),
            model: opts.model.clone(),
            codebase_hash: codebase_hash.clone(),
            codebase_dirty,
        })?;
    }

    // Append user_turn if task provided
    if let Some(task) = &opts.task {
        append_event(&tx_path, &AnyEvent::UserTurn {
            content: task.clone(),
            context: Some(crate::session::transcript::UserTurnContext {
                codebase_hash: Some(codebase_hash.clone()),
                codebase_dirty: Some(codebase_dirty),
                dirty_files: if dirty_files.is_empty() { None } else { Some(dirty_files.clone()) },
                injected_files: None,
            }),
            ts: now_ms(),
        })?;
    }

    let mut messages = reconstruct_messages(&tx_path)?;
    // Prepend turn context as first user message
    let turn_ctx = build_turn_context(&codebase_hash, &dirty_files);
    messages.insert(0, Message::User { content: MessageContent::Text(turn_ctx) });

    let mut total_usage = Usage::default();

    for _step in 0..50 {
        let mut chunk_stream = provider.stream(&opts.system_prompt, &messages, &tools).await
            .map_err(|e| anyhow::anyhow!("provider error: {}", e))?;

        let mut step_text = String::new();
        let mut step_tool_calls: Vec<ToolCall> = vec![];

        while let Some(chunk) = chunk_stream.next().await {
            match chunk.map_err(|e| anyhow::anyhow!("stream error: {}", e))? {
                StreamChunk::Text(t) => {
                    eprint!("{}", t);
                    step_text.push_str(&t);
                }
                StreamChunk::ToolCallComplete { tool_call_id, tool_name, args } => {
                    step_tool_calls.push(ToolCall { tool_call_id, tool_name, args });
                }
                StreamChunk::StepUsage(u) => {
                    total_usage = total_usage.add(&u);
                }
                StreamChunk::Done => break,
                _ => {}
            }
        }

        // Flush assistant text — only as a standalone event when there are NO tool calls.
        // When tool calls are present, the text is included as AssistantPart::Text in the
        // Parts message below. Writing a separate AssistantText event here would produce
        // two consecutive assistant messages in context reconstruction, which Anthropic rejects.
        if !step_text.is_empty() && step_tool_calls.is_empty() {
            append_event(&tx_path, &AnyEvent::AssistantText {
                content: step_text.clone(),
                ts: now_ms(),
            })?;
        }

        // No tool calls → turn complete
        if step_tool_calls.is_empty() {
            let events = read_events(&tx_path)?;
            let turn_n = count_turn_complete(&events) + 1;
            append_event(&tx_path, &AnyEvent::TurnComplete {
                usage: TurnUsage {
                    input: total_usage.input_tokens as u64,   // Usage uses u32; TurnUsage uses u64
                    output: total_usage.output_tokens as u64,
                    cache_read: 0,
                    cache_write: 0,
                },
                codebase_hash: codebase_hash.clone(),
                codebase_dirty,
                output: TurnOutput {
                    summary: step_text.lines().next().unwrap_or("").to_string(),
                    result: None,
                },
                ts: now_ms(),
            })?;
            git::commit_session(&opts.workspace, &opts.session_id,
                &format!("session({}): turn {}", opts.session_id, turn_n))?;
            eprintln!("\n[gait] turn {} complete", turn_n);
            return Ok(());
        }

        // Partition tool calls: allowed vs needs permission
        let mut pending_permissions: Vec<PermissionItem> = vec![];
        let mut tool_results: Vec<ToolResult> = vec![];
        let mut assistant_parts: Vec<AssistantPart> = vec![];

        if !step_text.is_empty() {
            assistant_parts.push(AssistantPart::Text { text: step_text.clone() });
        }

        // First pass: collect pending permissions for non-allowed tools
        for tc in &step_tool_calls {
            let allowed = opts.auto || opts.allow.contains(&tc.tool_name);
            if !allowed {
                pending_permissions.push(PermissionItem {
                    id: tc.tool_call_id.clone(),
                    cmd: format!("{}({})", tc.tool_name, tc.args),
                    reason: format!("{} requires permission", tc.tool_name),
                });
            }
        }

        // If non-auto and permissions needed: write event, commit, exit
        if !pending_permissions.is_empty() && !opts.auto {
            append_event(&tx_path, &AnyEvent::Permissions {
                items: pending_permissions,
                ts: now_ms(),
            })?;
            git::commit_session(&opts.workspace, &opts.session_id,
                &format!("session({}): permissions", opts.session_id))?;
            eprintln!("\n[gait] turn paused — permission approval required. re-run with --auto or approve interactively.");
            return Ok(());
        }

        // Execute all tool calls (in auto mode, write permissions audit trail)
        if !pending_permissions.is_empty() {
            // Auto mode: write permissions + auto-reply for audit
            append_event(&tx_path, &AnyEvent::Permissions {
                items: pending_permissions.clone(),
                ts: now_ms(),
            })?;
            append_event(&tx_path, &AnyEvent::PermissionsReply {
                from: "auto".into(),
                items: pending_permissions.iter().map(|p| PermissionsReplyItem {
                    id: p.id.clone(),
                    decision: "approve".into(),
                    text: None,
                }).collect(),
                ts: now_ms(),
            })?;
        }

        // Execute all tool calls
        for tc in &step_tool_calls {
            let id = tc.tool_call_id.clone();
            append_event(&tx_path, &AnyEvent::ToolCall {
                id: id.clone(),
                name: tc.tool_name.clone(),
                input: tc.args.clone(),
                ts: now_ms(),
            })?;
            assistant_parts.push(AssistantPart::ToolCall(ToolCall {
                tool_call_id: id.clone(),
                tool_name: tc.tool_name.clone(),
                args: tc.args.clone(),
            }));
            let output = match tools.iter().find(|t| t.name() == tc.tool_name) {
                Some(tool) => match tool.execute(tc.args.clone()).await {
                    Ok(v) => if v.is_string() { v.as_str().unwrap_or("").to_string() } else { v.to_string() },
                    Err(e) => format!("error: {}", e),
                },
                None => format!("error: unknown tool '{}'", tc.tool_name),
            };
            append_event(&tx_path, &AnyEvent::ToolResult {
                id: id.clone(),
                output: output.clone(),
                ts: now_ms(),
            })?;
            tool_results.push(ToolResult {
                kind: ToolResultKind::ToolResult,
                tool_call_id: id,
                result: serde_json::Value::String(output),
            });
        }

        // Append this step to messages for next iteration
        messages.push(Message::Assistant { content: AssistantContent::Parts(assistant_parts) });
        messages.push(Message::Tool { content: tool_results });
    }

    anyhow::bail!("reached max steps (50) without turn_complete")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use crate::provider::ProviderError;
    use crate::types::ToolDef;
    use async_stream::stream;

    fn init_git_repo(dir: &Path) {
        let repo = git2::Repository::init(dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        repo.find_branch("master", git2::BranchType::Local)
            .and_then(|mut b| { b.rename("main", false)?; Ok(()) }).ok();
    }

    struct MockProvider {
        responses: Arc<Vec<Vec<StreamChunk>>>,
        call_idx: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        fn model_id(&self) -> &str { "mock" }
        async fn stream(
            &self, _system: &str, _messages: &[Message], _tools: &[Box<dyn Tool>],
        ) -> Result<std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>, ProviderError> {
            let idx = self.call_idx.fetch_add(1, Ordering::SeqCst);
            let chunks = self.responses.get(idx).cloned().unwrap_or_default();
            Ok(Box::pin(stream! {
                for c in chunks { yield Ok(c); }
            }))
        }
    }

    #[tokio::test]
    async fn run_turn_writes_transcript_and_commits() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        // Codebase dir (can be same as workspace for test)
        let codebase = ws.path().to_path_buf();

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                // Step 1: call the read tool
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "read".into(),
                        args: serde_json::json!({ "file_path": "README.md" }),
                    },
                    StreamChunk::Done,
                ],
                // Step 2: final text response
                vec![
                    StreamChunk::Text("Task complete.".into()),
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let read_tool = ToolDef {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("file contents".into()))
            })),
        };

        run_turn(
            TurnOpts {
                session_id: "stest001".into(),
                task: Some("do the thing".into()),
                auto: true,
                workspace: ws.path().to_path_buf(),
                codebase_path: codebase,
                allow: vec!["read".into()],
                system_prompt: "you are a helpful assistant".into(),
                model: "mock/test".into(),
            },
            provider,
            vec![Box::new(read_tool)],
        ).await.unwrap();

        // Verify transcript has expected events
        let tx_path = ws.path().join("sessions/stest001/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();
        let types: Vec<&str> = events.iter().map(|e| match e {
            AnyEvent::SessionStart { .. } => "session_start",
            AnyEvent::UserTurn { .. } => "user_turn",
            AnyEvent::ToolCall { .. } => "tool_call",
            AnyEvent::ToolResult { .. } => "tool_result",
            AnyEvent::AssistantText { .. } => "assistant_text",
            AnyEvent::TurnComplete { .. } => "turn_complete",
            _ => "other",
        }).collect();
        assert!(types.contains(&"session_start"), "got: {:?}", types);
        assert!(types.contains(&"user_turn"), "got: {:?}", types);
        assert!(types.contains(&"tool_call"), "got: {:?}", types);
        assert!(types.contains(&"tool_result"), "got: {:?}", types);
        assert!(types.contains(&"turn_complete"), "got: {:?}", types);

        // Verify git commit was created
        let repo = git2::Repository::open(ws.path().join("sessions/stest001")).unwrap();
        let msg = repo.head().unwrap().peel_to_commit().unwrap().message().unwrap().to_string();
        assert!(msg.contains("turn 1"), "got: {msg}");
    }

    #[tokio::test]
    async fn permission_gating_pauses_turn_in_non_auto_mode() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "shell".into(), // not in allow list
                        args: serde_json::json!({ "cmd": "rm -rf /", "reason": "cleanup" }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        run_turn(
            TurnOpts {
                session_id: "stest002".into(),
                task: Some("do something dangerous".into()),
                auto: false, // non-auto
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec!["read".into()], // shell is NOT allowed
                system_prompt: "".into(),
                model: "mock/test".into(),
            },
            provider,
            vec![],
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/stest002/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();
        let has_permissions = events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. }));
        let has_turn_complete = events.iter().any(|e| matches!(e, AnyEvent::TurnComplete { .. }));
        assert!(has_permissions, "should have permissions event");
        assert!(!has_turn_complete, "should NOT have turn_complete — turn was paused");
    }
}
```

- [x] **Step 2: Add `libc` to Cargo.toml**

In `gait-rs/Cargo.toml` `[dependencies]`:

```toml
libc = "0.2"
```

- [x] **Step 3: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test turn::tests 2>&1
```

Expected: both turn tests pass.

- [x] **Step 4: Commit**

```bash
git add gait-rs/Cargo.toml gait-rs/src/turn.rs
git commit -m "feat(rs): run_turn — agentic loop with permission gate and transcript logging"
```

---

## Task 6: Wire `src/lib.rs` + `src/bin/gait.rs`

**Files:**
- Modify: `gait-rs/src/lib.rs`
- Create: `gait-rs/src/bin/gait.rs`

- [x] **Step 1: Update `src/lib.rs`**

Add module declarations at the top:

```rust
pub mod git;
pub mod session;
pub mod config;
pub mod auth;
pub mod turn;
// existing modules below:
pub mod types;
pub mod provider;
pub mod anthropic;
pub mod openai;
pub mod stream;
pub mod generate;
pub mod factory;
pub mod tools;

pub use tools::builtin_tools;
pub use turn::{TurnOpts, run_turn};
```

(Keep the existing re-exports; just add the new `pub mod` lines and re-exports.)

- [x] **Step 2: Create `src/bin/gait.rs`**

```rust
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
            "--session" => { i += 1; session_id = argv.get(i).cloned(); }
            "--task"    => { i += 1; task = argv.get(i).cloned(); }
            "--workspace" => { i += 1; workspace = argv.get(i).map(PathBuf::from).unwrap_or(workspace); }
            "--codebase"  => { i += 1; codebase_path = argv.get(i).map(PathBuf::from); }
            "--model"   => { i += 1; model_override = argv.get(i).cloned(); }
            "--auto"    => { auto = true; }
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
    let codebase_path = codebase_path
        .unwrap_or_else(|| workspace.join("codebase"));

    // Resolve API key from provider portion of model string (e.g. "anthropic" from "anthropic/...")
    let provider_name = model.split('/').next().unwrap_or("anthropic");
    let api_key = resolve_api_key(provider_name);

    let provider = match load_provider(&model, api_key) {  // api_key is Option<String> — matches factory signature
        Ok(p) => Arc::from(p),
        Err(e) => {
            eprintln!("[gait] error: could not load provider for '{}': {}", model, e);
            std::process::exit(1);
        }
    };

    // Read system prompt from AGENTS.md if present
    let agents_md = workspace.join("AGENTS.md");
    let system_prompt = std::fs::read_to_string(&agents_md).unwrap_or_else(|_| {
        "You are a helpful AI coding assistant. You have access to tools to read, write, and edit files in the codebase.".into()
    });

    let tools = builtin_tools(codebase_path.clone());

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
```

- [x] **Step 3: Verify the binary compiles**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo build --bin gait 2>&1
```

Expected: compiles without errors. Fix any type errors before continuing.

- [x] **Step 4: Smoke test the binary help output**

```bash
./target/debug/gait 2>&1 || true
```

Expected: prints usage line and exits 1.

- [x] **Step 5: Run full test suite to verify nothing regressed**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test 2>&1 | tail -20
```

Expected: all existing tests still pass plus new tests.

- [x] **Step 6: Commit**

```bash
git add gait-rs/src/lib.rs gait-rs/src/bin/gait.rs
git commit -m "feat(rs): gait binary — run command wires config, auth, provider, turn loop"
```

---

## Task 7: Integration test

**Files:**
- Create: `gait-rs/tests/integration_turn.rs`

End-to-end test: starts a fresh git workspace, runs a full turn with a mock provider (one tool call + final text), verifies transcript JSONL and git commit.

- [x] **Step 1: Create `gait-rs/tests/integration_turn.rs`**

```rust
use gait_ai::{
    turn::{TurnOpts, run_turn},
    session::transcript::{read_events, AnyEvent},
};
use gait_ai::provider::{Provider, StreamChunk, ProviderError};
use gait_ai::types::{Message, Tool, ToolDef};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use tempfile::TempDir;
use async_stream::stream;

fn init_git_repo(dir: &std::path::Path) {
    let repo = git2::Repository::init(dir).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
    repo.find_branch("master", git2::BranchType::Local)
        .and_then(|mut b| { b.rename("main", false)?; Ok(()) }).ok();
}

struct ScriptedProvider {
    calls: Arc<Vec<Vec<StreamChunk>>>,
    idx: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn model_id(&self) -> &str { "scripted" }
    async fn stream(
        &self, _s: &str, _m: &[Message], _t: &[Box<dyn Tool>],
    ) -> Result<std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>, ProviderError> {
        let i = self.idx.fetch_add(1, Ordering::SeqCst);
        let chunks = self.calls.get(i).cloned().unwrap_or_default();
        Ok(Box::pin(stream! { for c in chunks { yield Ok(c); } }))
    }
}

#[tokio::test]
async fn full_turn_creates_transcript_and_git_commit() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    std::fs::write(ws.path().join("README.md"), "hello").unwrap();

    let provider = Arc::new(ScriptedProvider {
        calls: Arc::new(vec![
            // First LLM step: call ls tool
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "id1".into(),
                    tool_name: "ls".into(),
                    args: serde_json::json!({}),
                },
                StreamChunk::Done,
            ],
            // Second LLM step: final answer
            vec![
                StreamChunk::Text("I listed the files. Done.".into()),
                StreamChunk::Done,
            ],
        ]),
        idx: Arc::new(AtomicUsize::new(0)),
    });

    let ls_tool = ToolDef {
        name: "ls".into(),
        description: "list files".into(),
        parameters: serde_json::json!({ "type": "object" }),
        execute: Box::new(|_| Box::pin(async { Ok(serde_json::Value::String("README.md".into())) })),
    };

    run_turn(
        TurnOpts {
            session_id: "sint001".into(),
            task: Some("list the files".into()),
            auto: true,
            workspace: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            allow: vec!["ls".into()],
            system_prompt: "you are helpful".into(),
            model: "mock/test".into(),
        },
        provider,
        vec![Box::new(ls_tool)],
    ).await.unwrap();

    // Verify transcript
    let tx = ws.path().join("sessions/sint001/transcript.jsonl");
    let events = read_events(&tx).unwrap();
    let event_types: Vec<String> = events.iter().map(|e| match e {
        AnyEvent::SessionStart { .. } => "session_start",
        AnyEvent::UserTurn { .. } => "user_turn",
        AnyEvent::ToolCall { .. } => "tool_call",
        AnyEvent::ToolResult { .. } => "tool_result",
        AnyEvent::AssistantText { .. } => "assistant_text",
        AnyEvent::TurnComplete { .. } => "turn_complete",
        _ => "other",
    }.to_string()).collect();

    assert!(event_types.contains(&"session_start".to_string()), "missing session_start: {:?}", event_types);
    assert!(event_types.contains(&"user_turn".to_string()), "missing user_turn: {:?}", event_types);
    assert!(event_types.contains(&"tool_call".to_string()), "missing tool_call: {:?}", event_types);
    assert!(event_types.contains(&"tool_result".to_string()), "missing tool_result: {:?}", event_types);
    assert!(event_types.contains(&"assistant_text".to_string()), "missing assistant_text: {:?}", event_types);
    assert!(event_types.contains(&"turn_complete".to_string()), "missing turn_complete: {:?}", event_types);

    // Verify transcript is valid JSONL (each line is valid JSON)
    let raw = std::fs::read_to_string(&tx).unwrap();
    for line in raw.lines().filter(|l| !l.is_empty()) {
        serde_json::from_str::<serde_json::Value>(line).expect("invalid JSON line: {line}");
    }

    // Verify git commit in session branch
    let session_repo = git2::Repository::open(ws.path().join("sessions/sint001")).unwrap();
    let commit_msg = session_repo.head().unwrap().peel_to_commit().unwrap()
        .message().unwrap().to_string();
    assert!(commit_msg.contains("turn 1"), "unexpected commit message: {commit_msg}");
}

#[tokio::test]
async fn second_turn_resumes_session() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(ScriptedProvider {
        calls: Arc::new(vec![
            vec![StreamChunk::Text("Turn 1 done.".into()), StreamChunk::Done],
            vec![StreamChunk::Text("Turn 2 done.".into()), StreamChunk::Done],
        ]),
        idx: Arc::new(AtomicUsize::new(0)),
    });

    // First turn
    run_turn(TurnOpts {
        session_id: "sint002".into(), task: Some("task 1".into()), auto: true,
        workspace: ws.path().to_path_buf(), codebase_path: ws.path().to_path_buf(),
        allow: vec![], system_prompt: "".into(), model: "mock/test".into(),
    }, provider.clone(), vec![]).await.unwrap();

    // Second turn — resumes same session
    run_turn(TurnOpts {
        session_id: "sint002".into(), task: Some("task 2".into()), auto: true,
        workspace: ws.path().to_path_buf(), codebase_path: ws.path().to_path_buf(),
        allow: vec![], system_prompt: "".into(), model: "mock/test".into(),
    }, provider, vec![]).await.unwrap();

    let tx = ws.path().join("sessions/sint002/transcript.jsonl");
    let events = read_events(&tx).unwrap();
    let turn_count = events.iter().filter(|e| matches!(e, AnyEvent::TurnComplete { .. })).count();
    assert_eq!(turn_count, 2, "expected 2 turn_complete events");

    // Session start written only once
    let session_start_count = events.iter().filter(|e| matches!(e, AnyEvent::SessionStart { .. })).count();
    assert_eq!(session_start_count, 1, "session_start should appear exactly once");
}
```

- [x] **Step 2: Run integration tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test --test integration_turn 2>&1
```

Expected: both integration tests pass.

- [x] **Step 3: Run complete test suite**

```bash
cd /Users/aicoder/Documents/ideation/gait/.claude/worktrees/rs-port/gait-rs
cargo test 2>&1 | tail -5
```

Expected: all tests pass (should be 70+).

- [x] **Step 4: Commit**

```bash
git add gait-rs/tests/integration_turn.rs
git commit -m "test(rs): integration tests for full turn — transcript events + git commit"
```

---

## Self-Review

**Spec coverage:**
- ✅ git2 for all git ops (worktree create, commit, list sessions, hash, dirty)
- ✅ All 18 JSONL event types
- ✅ Message reconstruction with compaction breakpoint
- ✅ Permission gating (allow list, auto mode, non-auto exits cleanly)
- ✅ Session lock (RAII PID file)
- ✅ `gait run` CLI command
- ✅ Config loading from `gait.jsonc` (JSONC comment stripping)
- ✅ API key resolution (env var + auth.json)
- ✅ Session-compatible JSONL format with TypeScript gait
- ⏭️ Compaction — Phase 4
- ⏭️ Clarify/inform interactive flow — Phase 4
- ⏭️ `gait sessions`, `gait finish`, `gait auth`, `gait init` commands — Phase 4

**Placeholder scan:** No TBDs, no "implement later" — all code is complete.

**Type consistency check:**
- `AnyEvent::ToolCall { id, name, input, ts }` — used consistently in transcript.rs, context.rs, turn.rs
- `TurnOpts` struct — defined in turn.rs, used in bin/gait.rs as `gait_ai::turn::TurnOpts`
- `SessionLock::acquire(session_dir: &Path)` — takes `&Path`, called with `&session_dir` in run_turn
- `git::commit_session(workspace, session_id, msg)` — all call sites match this signature
- `reconstruct_messages(path: &Path)` — called in turn.rs with `&tx_path: PathBuf`; `.as_path()` or `&tx_path` both work since `PathBuf: Deref<Target=Path>`
- `Usage::input_tokens` / `output_tokens` — matches types.rs field names used in turn.rs TurnUsage
