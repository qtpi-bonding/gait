---
status: partial
---

# Session Worktrees, System Prompt + Init Decoupling — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give each gait session its own git branch + worktree for independent rollback; add a system prompt with injected context each turn; decouple `gait init` from the submodule assumption.

**Architecture:**
- Workspace `main` branch holds only `gait.jsonc`, `AGENTS.md`, `.gitignore`
- Each session gets a git worktree at `sessions/<id>/` on orphan branch `session/<id>`
- Session commits happen inside the worktree — independent of main and every other session
- System prompt is split: **stable** content (session ID, paths, AGENTS.md) goes in `system`; **volatile** content (timestamp, git state) goes as a prepended user message each turn — preserves provider-side prompt caching
- Codebase stays clean — no gait files in it

**Workspace vs codebase separation:**
- **Workspace** — `gait.jsonc`, `AGENTS.md`, `sessions/` — all agent-related files live here
- **Codebase** — untouched external repo; gait reads/writes code there but never puts gait files in it

**Tech Stack:** TypeScript, Bun, git CLI (**≥ 2.42** — `git worktree add --orphan` landed in 2.42.0, Aug 2023)

**Key design decisions:**
- Subagent sessions (`parent-sub-<id>`) get their own worktrees — separate sessions, independent rollback
- `commitSession` skips if no changes; detects non-worktree session dirs and falls back to workspace-level commit (legacy compat)
- `git worktree add` protected by atomic workspace-level lock using `openSync("wx")` + stale-PID recovery
- `removeSessionWorktree` must only be called when no turn is in progress (documented constraint; enforced in future `gait session` subcommand)
- `codebase_path` in `session.json` is write-once — never silently rebound mid-session
- `isCodebaseDirty` implemented via `getCodebaseDirtyFiles` to avoid duplicate `git status` calls
- `AGENTS.md` created by `gait init` with a short stub describing gait; users extend it with project-specific instructions

**Requires reading before starting:**
- `src/git.ts` — all functions
- `src/cli/cmd/init.ts` — `initCommand`, `DEFAULT_CONFIG`
- `src/cli/cmd/run.ts` — `ensureSessionDir`, `runTurn`, `updateSessionJson`, `initSession` call sites
- `src/lock.ts` — `acquireLock`, `releaseLock`, `isProcessAlive`

---

## File Map

| File | Change |
|---|---|
| `src/git.ts` | Add `createSessionWorktree`, `removeSessionWorktree`, `checkGitVersion`, `getCodebaseDirtyFiles`; update `commitSession`, `initialCommit`, `isCodebaseDirty` |
| `src/system_prompt.ts` | New — `buildSystemPrompt` (stable) + `buildTurnContext` (volatile) |
| `src/system_prompt.test.ts` | New — tests for both builders |
| `src/cli/cmd/init.ts` | Create `AGENTS.md` stub, update `.gitignore`, update output messages, git version check |
| `src/cli/cmd/run.ts` | Wire worktrees, system prompt, turn context, `codebase_path` persistence, fix `initSession` call |
| `src/git.test.ts` | New — tests for all new/changed git functions |

---

## Task 1: Update `gait init`

**Files:** `src/cli/cmd/init.ts`, `src/git.ts`

- [x] **Step 1: Add `checkGitVersion` to `src/git.ts`**

Replaces `checkGitInstalled`:

```typescript
export function checkGitVersion(): void {
  let output: string
  try {
    output = run("git --version", process.cwd())
  } catch {
    throw new Error("git is not installed or not in PATH. gait requires git.")
  }
  const match = output.match(/(\d+)\.(\d+)\.(\d+)/)
  if (!match) throw new Error(`Could not parse git version from: ${output}`)
  const [, major, minor] = match.map(Number)
  if (major! < 2 || (major === 2 && minor! < 42)) {
    throw new Error(
      `gait requires git >= 2.42 (for git worktree add --orphan). Found: ${output.trim()}`
    )
  }
}
```

- [x] **Step 2: Fix `initialCommit` in `src/git.ts`**

`git add -A` would stage session worktree dirs. Commit only known files; handle AGENTS.md being absent:

```typescript
export function initialCommit(workspacePath: string): void {
  // Stage known workspace files; AGENTS.md may or may not exist
  const toStage = ["gait.jsonc", ".gitignore"]
  const agentsPath = path.join(workspacePath, "AGENTS.md")
  if (fs.existsSync(agentsPath)) toStage.push("AGENTS.md")
  run(`git add ${toStage.join(" ")}`, workspacePath)
  run('git commit -m "gait: init workspace"', workspacePath)
}
```

- [x] **Step 3: Update `.gitignore` content in `init.ts`**

```
sessions/
.gait-ws.lock
```

Verify no code still references `.gait-git.lock` (grep the codebase — remove or rename any remaining references).

- [x] **Step 4: Create the `AGENTS.md` stub in `init.ts`**

Write this file to `workspacePath/AGENTS.md` if it does not already exist:

```markdown
# gait agent

You are a coding agent running inside gait, a stateless turn-based agent framework.

**How it works:**
- Each `gait run` invocation is one turn. Your full conversation history is replayed from a transcript each turn.
- The `<gait>` block in your context shows your session ID, workspace, and codebase path. A `<gait-state>` block shows the current timestamp and codebase git status.
- The codebase is the external repo you work on. The workspace holds gait config and session history — keep the codebase clean.

**Tools:**
- `read`, `write`, `edit`, `multiedit`, `apply_patch`, `glob`, `grep`, `ls`, `codesearch`, `shell`, `webfetch` — work on the codebase
- Tools not in the allow list require user approval before running
- `questions` — ask the user clarifying questions; pauses the turn until they answer on the next invocation
- `spawn_subagent` — delegate a sub-task to a child session

**Turn model:**
- Complete your task in as few turns as possible
- Read files rather than asking when you need information
- Use `questions` only when blocked on a decision only the user can make
```

- [x] **Step 5: Update `initCommand`**

Replace `checkGitInstalled()` with `checkGitVersion()`. Add AGENTS.md creation (step 4). Update output messages:

No URL:
```
gait workspace initialized

Start an agent turn:
  gait run --task "..."

To point at a codebase:
  gait run --codebase /path/to/repo --task "..."

Edit AGENTS.md to add project-specific instructions for the agent.
```

With URL (submodule, kept as opt-in):
```
gait workspace initialized with codebase submodule at <url>

Start an agent turn:
  gait run --task "..."
```

- [x] **Step 6: Run existing tests**

```bash
cd /Users/aicoder/Documents/ideation/gait && bun test
```
Expected: 68 pass, 0 fail

- [x] **Step 7: Commit**

```bash
git add src/git.ts src/cli/cmd/init.ts
git commit -m "feat(init): AGENTS.md stub, git version check, decouple from submodule"
```

---

## Task 2: Add `createSessionWorktree`, `removeSessionWorktree`, `getCodebaseDirtyFiles`

**Files:** `src/git.ts`, `src/git.test.ts`

- [x] **Step 1: Write failing tests in `src/git.test.ts`**

```typescript
import { describe, it, expect, beforeEach, afterEach } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { execSync } from "child_process"
import { createSessionWorktree, removeSessionWorktree, commitSession, getCodebaseDirtyFiles } from "./git"

let tmpDir: string

function initRepo(dir: string): void {
  execSync("git init", { cwd: dir })
  execSync("git config user.email 'test@test.com'", { cwd: dir })
  execSync("git config user.name 'Test'", { cwd: dir })
  execSync("touch gait.jsonc && git add gait.jsonc && git commit -m 'init'", { cwd: dir })
}

beforeEach(() => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "gait-git-"))
  initRepo(tmpDir)
})

afterEach(() => {
  try { execSync("git worktree prune", { cwd: tmpDir }) } catch {}
  fs.rmSync(tmpDir, { recursive: true, force: true })
})

describe("createSessionWorktree", () => {
  it("creates worktree dir at sessions/<id>/", () => {
    createSessionWorktree(tmpDir, "s1234")
    expect(fs.existsSync(path.join(tmpDir, "sessions", "s1234"))).toBe(true)
  })

  it(".git inside worktree is a file not a dir", () => {
    createSessionWorktree(tmpDir, "s1234")
    const gitPath = path.join(tmpDir, "sessions", "s1234", ".git")
    expect(fs.existsSync(gitPath)).toBe(true)
    expect(fs.statSync(gitPath).isFile()).toBe(true)
  })

  it("worktree is on branch session/s1234", () => {
    createSessionWorktree(tmpDir, "s1234")
    const branch = execSync("git rev-parse --abbrev-ref HEAD", {
      cwd: path.join(tmpDir, "sessions", "s1234"), encoding: "utf-8"
    }).trim()
    expect(branch).toBe("session/s1234")
  })

  it("creates .gitignore that excludes lock", () => {
    createSessionWorktree(tmpDir, "s1234")
    const gi = fs.readFileSync(path.join(tmpDir, "sessions", "s1234", ".gitignore"), "utf-8")
    expect(gi).toContain("lock")
  })

  it("makes an initial commit on the session branch", () => {
    createSessionWorktree(tmpDir, "s1234")
    const log = execSync("git log --oneline", {
      cwd: path.join(tmpDir, "sessions", "s1234"), encoding: "utf-8"
    }).trim()
    expect(log.length).toBeGreaterThan(0)
    expect(log).toContain("s1234")
  })

  it("is idempotent — calling twice does not throw", () => {
    createSessionWorktree(tmpDir, "s1234")
    expect(() => createSessionWorktree(tmpDir, "s1234")).not.toThrow()
  })

  it("idempotent call does not create a duplicate worktree entry", () => {
    createSessionWorktree(tmpDir, "s1234")
    createSessionWorktree(tmpDir, "s1234")
    const list = execSync("git worktree list", { cwd: tmpDir, encoding: "utf-8" })
    const matches = list.match(/session\/s1234/g)
    expect(matches).toHaveLength(1)
  })

  it("does not affect the main workspace branch", () => {
    createSessionWorktree(tmpDir, "s1234")
    const branch = execSync("git rev-parse --abbrev-ref HEAD", { cwd: tmpDir, encoding: "utf-8" }).trim()
    expect(["main", "master"]).toContain(branch)
  })

  it("recovers from stale worktree (dir removed without git worktree remove)", () => {
    createSessionWorktree(tmpDir, "s1234")
    fs.rmSync(path.join(tmpDir, "sessions", "s1234"), { recursive: true, force: true })
    expect(() => createSessionWorktree(tmpDir, "s1234")).not.toThrow()
    expect(fs.existsSync(path.join(tmpDir, "sessions", "s1234"))).toBe(true)
  })
})

describe("removeSessionWorktree", () => {
  it("removes the worktree dir and admin entry", () => {
    createSessionWorktree(tmpDir, "s1234")
    removeSessionWorktree(tmpDir, "s1234")
    expect(fs.existsSync(path.join(tmpDir, "sessions", "s1234"))).toBe(false)
    const list = execSync("git worktree list", { cwd: tmpDir, encoding: "utf-8" })
    expect(list).not.toContain("s1234")
  })

  it("is safe to call when worktree does not exist", () => {
    expect(() => removeSessionWorktree(tmpDir, "nonexistent")).not.toThrow()
  })
})

describe("commitSession", () => {
  it("commits files to the session branch", () => {
    createSessionWorktree(tmpDir, "s9999")
    fs.writeFileSync(path.join(tmpDir, "sessions", "s9999", "transcript.jsonl"), '{"ts":1}\n')
    commitSession(tmpDir, "s9999", "session(s9999): turn 1")
    const log = execSync("git log --oneline", {
      cwd: path.join(tmpDir, "sessions", "s9999"), encoding: "utf-8"
    }).trim()
    expect(log).toContain("turn 1")
  })

  it("does not affect the main workspace branch", () => {
    createSessionWorktree(tmpDir, "s9999")
    fs.writeFileSync(path.join(tmpDir, "sessions", "s9999", "transcript.jsonl"), '{"ts":1}\n')
    commitSession(tmpDir, "s9999", "session(s9999): turn 1")
    const mainLog = execSync("git log --oneline", { cwd: tmpDir, encoding: "utf-8" }).trim()
    expect(mainLog).not.toContain("turn 1")
  })

  it("does not throw when there are no changes (read-only turn)", () => {
    createSessionWorktree(tmpDir, "s9999")
    expect(() => commitSession(tmpDir, "s9999", "session(s9999): turn 1")).not.toThrow()
  })
})

describe("getCodebaseDirtyFiles", () => {
  it("returns empty array for a clean repo", () => {
    expect(getCodebaseDirtyFiles(tmpDir)).toEqual([])
  })

  it("returns modified tracked files", () => {
    fs.writeFileSync(path.join(tmpDir, "gait.jsonc"), "modified")
    const files = getCodebaseDirtyFiles(tmpDir)
    expect(files.some(f => f.includes("gait.jsonc"))).toBe(true)
  })

  it("returns untracked files", () => {
    fs.writeFileSync(path.join(tmpDir, "new-file.ts"), "hello")
    const files = getCodebaseDirtyFiles(tmpDir)
    expect(files.some(f => f.includes("new-file.ts"))).toBe(true)
  })

  it("returns empty array if path is not a git repo", () => {
    const notARepo = fs.mkdtempSync(path.join(os.tmpdir(), "not-a-repo-"))
    expect(getCodebaseDirtyFiles(notARepo)).toEqual([])
    fs.rmSync(notARepo, { recursive: true })
  })
})
```

- [x] **Step 2: Run tests to confirm they fail**

```bash
bun test src/git.test.ts
```
Expected: FAIL

- [x] **Step 3: Implement all functions in `src/git.ts`**

Add `import fs from "fs"`, `import path from "path"`, `import { execSync, execFileSync } from "child_process"`.

```typescript
// Atomic lock using exclusive-create — avoids TOCTOU of existsSync+write
function withWorkspaceLock(workspacePath: string, fn: () => void): void {
  const lockPath = path.join(workspacePath, ".gait-ws.lock")
  const deadline = Date.now() + 10_000

  while (true) {
    try {
      // O_EXCL: fails with EEXIST if another process holds the lock
      const fd = fs.openSync(lockPath, "wx")
      fs.writeSync(fd, String(process.pid))
      fs.closeSync(fd)
      break
    } catch (e: any) {
      if (e.code !== "EEXIST") throw e

      // Stale lock check — same pattern as session lock in lock.ts
      try {
        const pid = parseInt(fs.readFileSync(lockPath, "utf-8").trim(), 10)
        if (!isNaN(pid)) {
          try { process.kill(pid, 0) } catch { fs.unlinkSync(lockPath); continue }
        }
      } catch { /* lock file disappeared — retry */ continue }

      if (Date.now() > deadline) throw new Error("Timed out waiting for workspace lock (.gait-ws.lock)")
      Bun.sleepSync(50)
    }
  }

  try { fn() } finally {
    try { fs.unlinkSync(lockPath) } catch {}
  }
}

function isExistingWorktree(workspacePath: string, sessionId: string): boolean {
  try {
    const list = run("git worktree list --porcelain", workspacePath)
    const sessionDir = path.resolve(path.join(workspacePath, "sessions", sessionId))
    return list.includes(sessionDir)
  } catch { return false }
}

export function createSessionWorktree(workspacePath: string, sessionId: string): void {
  if (isExistingWorktree(workspacePath, sessionId)) return

  const sessionDir = path.join(workspacePath, "sessions", sessionId)
  const branch = `session/${sessionId}`

  withWorkspaceLock(workspacePath, () => {
    if (isExistingWorktree(workspacePath, sessionId)) return

    try { run("git worktree prune", workspacePath) } catch {}

    // Graceful fallback for pre-existing non-worktree dirs (legacy sessions)
    if (fs.existsSync(sessionDir) && fs.readdirSync(sessionDir).length > 0) {
      process.stderr.write(
        `[gait] warning: session ${sessionId} predates worktree support; rollback unavailable\n`
      )
      return
    }

    fs.mkdirSync(path.join(workspacePath, "sessions"), { recursive: true })
    run(`git worktree add --orphan -b ${branch} sessions/${sessionId}`, workspacePath)
    fs.writeFileSync(path.join(sessionDir, ".gitignore"), "lock\n", "utf-8")
    run("git add .gitignore", sessionDir)
    run(`git commit -m "session(${sessionId}): init"`, sessionDir)
  })
}

// NOTE: must only be called when no turn is in-progress for this session.
// The session lock (sessions/<id>/lock) must not be held. Future gait session
// subcommand should acquire + check the lock before calling this.
export function removeSessionWorktree(workspacePath: string, sessionId: string): void {
  withWorkspaceLock(workspacePath, () => {
    if (!isExistingWorktree(workspacePath, sessionId)) return
    try {
      run(`git worktree remove --force sessions/${sessionId}`, workspacePath)
    } catch {
      try { run("git worktree prune", workspacePath) } catch {}
    }
  })
}

export function commitSession(workspacePath: string, sessionId: string, message: string): void {
  const sessionDir = path.join(workspacePath, "sessions", sessionId)
  const gitFile = path.join(sessionDir, ".git")

  // Detect non-worktree session (legacy / fallback path) — commit from workspace root
  if (!fs.existsSync(gitFile) || !fs.statSync(gitFile).isFile()) {
    try {
      run(`git add sessions/${sessionId}/`, workspacePath)
      const status = run("git status --porcelain", workspacePath)
      if (!status) return
      execFileSync("git", ["commit", "-m", message], { cwd: workspacePath, encoding: "utf-8" })
    } catch (err) { throw new Error(`git commit failed (legacy): ${err}`) }
    return
  }

  // Normal worktree path
  try {
    const status = run("git status --porcelain", sessionDir)
    if (!status) return
    run("git add -A", sessionDir)
    execFileSync("git", ["commit", "-m", message], { cwd: sessionDir, encoding: "utf-8" })
  } catch (err) { throw new Error(`git commit failed: ${err}`) }
}

export function getCodebaseDirtyFiles(codebasePath: string): string[] {
  try {
    const out = run("git status --porcelain", codebasePath)
    if (!out) return []
    return out.split("\n").filter(Boolean)
  } catch { return [] }
}

// Implemented via getCodebaseDirtyFiles to avoid a duplicate git status call
export function isCodebaseDirty(codebasePath: string): boolean {
  return getCodebaseDirtyFiles(codebasePath).length > 0
}
```

Note: the `isCodebaseDirty` export replaces the old implementation. All existing callers continue to work. `getCodebaseDirtyFiles` is called once per turn in `runTurn` and the result passed to both `buildTurnContext` and `turn_complete` event.

- [x] **Step 4: Run tests**

```bash
bun test src/git.test.ts && bun test
```
Expected: all pass

- [x] **Step 5: Commit**

```bash
git add src/git.ts src/git.test.ts
git commit -m "feat(git): session worktrees, getCodebaseDirtyFiles, atomic workspace lock, fix commitSession"
```

---

## Task 3: Build the system prompt

**Files:** `src/system_prompt.ts`, `src/system_prompt.test.ts`

Split into two functions to preserve prompt caching:
- **`buildSystemPrompt`** — stable content only (session ID, paths, AGENTS.md). Passed as `system` to `streamText`. Does not change turn-to-turn within a session.
- **`buildTurnContext`** — volatile content (timestamp, git hash, dirty files). Injected as a user message prepended to `messages` each turn.

This way the provider can cache `system` across turns of the same session.

System prompt format:
```
<gait>
session: s1a2b3c4
workspace: /path/to/ws
codebase: /path/to/repo
</gait>

<agents>
[contents of AGENTS.md — block omitted entirely if file absent]
</agents>
```

Turn context format (prepended as user message):
```
<gait-state>
timestamp: 1749187234
git: abc1234 | clean
</gait-state>
```

When dirty:
```
<gait-state>
timestamp: 1749187234
git: abc1234 | dirty
  M src/auth.ts
  ? src/new-file.ts
</gait-state>
```

- [x] **Step 1: Write failing tests in `src/system_prompt.test.ts`**

```typescript
import { describe, it, expect } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { buildSystemPrompt, buildTurnContext } from "./system_prompt"

describe("buildSystemPrompt", () => {
  it("contains <gait> block with session, workspace, codebase", () => {
    const result = buildSystemPrompt({
      sessionId: "s1234",
      workspacePath: "/ws",
      codebasePath: "/cb",
    })
    expect(result).toContain("<gait>")
    expect(result).toContain("session: s1234")
    expect(result).toContain("workspace: /ws")
    expect(result).toContain("codebase: /cb")
    expect(result).toContain("</gait>")
  })

  it("does not contain timestamp or git state", () => {
    const result = buildSystemPrompt({ sessionId: "s1", workspacePath: "/ws", codebasePath: "/cb" })
    expect(result).not.toContain("timestamp")
    expect(result).not.toContain("git:")
  })

  it("omits <agents> block when AGENTS.md does not exist", () => {
    const result = buildSystemPrompt({ sessionId: "s1", workspacePath: "/nonexistent", codebasePath: "/cb" })
    expect(result).not.toContain("<agents>")
  })

  it("includes <agents> block when AGENTS.md exists", () => {
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "gait-sp-"))
    fs.writeFileSync(path.join(tmp, "AGENTS.md"), "You are a Rust expert.")
    const result = buildSystemPrompt({ sessionId: "s1", workspacePath: tmp, codebasePath: "/cb" })
    expect(result).toContain("<agents>")
    expect(result).toContain("You are a Rust expert.")
    expect(result).toContain("</agents>")
    fs.rmSync(tmp, { recursive: true })
  })
})

describe("buildTurnContext", () => {
  it("contains <gait-state> with timestamp and git hash", () => {
    const result = buildTurnContext({ codebaseHash: "abc123", dirtyFiles: [], timestamp: 9999 })
    expect(result).toContain("<gait-state>")
    expect(result).toContain("timestamp: 9999")
    expect(result).toContain("git: abc123 | clean")
    expect(result).toContain("</gait-state>")
  })

  it("shows clean when no dirty files", () => {
    const result = buildTurnContext({ codebaseHash: "abc", dirtyFiles: [], timestamp: 1 })
    expect(result).toContain("| clean")
    expect(result).not.toContain("| dirty")
  })

  it("shows dirty files when present", () => {
    const result = buildTurnContext({
      codebaseHash: "abc", timestamp: 1,
      dirtyFiles: [" M src/auth.ts", "?? src/new.ts"],
    })
    expect(result).toContain("| dirty")
    expect(result).toContain("M src/auth.ts")
    expect(result).toContain("? src/new.ts")
  })
})
```

- [x] **Step 2: Run to confirm failure**

```bash
bun test src/system_prompt.test.ts
```
Expected: FAIL

- [x] **Step 3: Implement `src/system_prompt.ts`**

```typescript
import fs from "fs"
import path from "path"

interface SystemPromptOpts {
  sessionId: string
  workspacePath: string
  codebasePath: string
}

interface TurnContextOpts {
  codebaseHash: string
  dirtyFiles: string[]
  timestamp: number
}

export function buildSystemPrompt(opts: SystemPromptOpts): string {
  const { sessionId, workspacePath, codebasePath } = opts

  const gaitBlock = `<gait>
session: ${sessionId}
workspace: ${workspacePath}
codebase: ${codebasePath}
</gait>`

  const agentsPath = path.join(workspacePath, "AGENTS.md")
  if (!fs.existsSync(agentsPath)) return gaitBlock

  const agentsContent = fs.readFileSync(agentsPath, "utf-8").trim()
  return `${gaitBlock}\n\n<agents>\n${agentsContent}\n</agents>`
}

export function buildTurnContext(opts: TurnContextOpts): string {
  const { codebaseHash, dirtyFiles, timestamp } = opts

  const gitLine = dirtyFiles.length === 0
    ? `git: ${codebaseHash} | clean`
    : `git: ${codebaseHash} | dirty\n${dirtyFiles.map(f => `  ${f.trim()}`).join("\n")}`

  return `<gait-state>\ntimestamp: ${timestamp}\n${gitLine}\n</gait-state>`
}
```

- [x] **Step 4: Run tests**

```bash
bun test src/system_prompt.test.ts && bun test
```
Expected: all pass

- [x] **Step 5: Commit**

```bash
git add src/system_prompt.ts src/system_prompt.test.ts
git commit -m "feat: system prompt (stable) + turn context (volatile) for LLM injection"
```

---

## Task 4: Wire everything into `runTurn`

**Files:** `src/cli/cmd/run.ts`

Four wiring changes: session worktree creation, `codebase_path` persistence (write-once), fix `initSession` to use resolved `codebasePath`, and inject system prompt + turn context into `streamText`.

- [x] **Step 1: Wire session worktree — replace `ensureSessionDir`**

Change `ensureSessionDir` to a pure path helper (no mkdir — worktree creation owns the dir):

```typescript
function sessionDirPath(workspacePath: string, sessionId: string): string {
  return path.join(workspacePath, "sessions", sessionId)
}
```

In `runTurn`:
```typescript
const dir = sessionDirPath(workspacePath, sessionId)
createSessionWorktree(workspacePath, sessionId)
acquireLock(dir)
```

Update all subsequent uses of the old `sessionDir` variable to `dir`.

Add `createSessionWorktree` to git imports.

- [x] **Step 2: Add `codebase_path` to `session.json` — write-once**

Add a helper to read it back:

```typescript
function readCodebasePath(workspacePath: string, sessionId: string): string | undefined {
  const p = sessionJsonPath(workspacePath, sessionId)
  if (!fs.existsSync(p)) return undefined
  try { return JSON.parse(fs.readFileSync(p, "utf-8")).codebase_path } catch { return undefined }
}
```

Resolve `codebasePath` using three-tier fallback (before `acquireLock`):

```typescript
const codebasePath = args.codebasePath
  ?? readCodebasePath(workspacePath, sessionId)
  ?? path.join(workspacePath, "codebase")
```

Update `updateSessionJson` — write `codebase_path` only on first write (when session.json does not yet exist):

```typescript
function updateSessionJson(
  workspacePath: string, sessionId: string,
  codebasePath: string,
  usageDelta: { input: number; output: number; cache_read: number; cache_write: number }
): void {
  const p = sessionJsonPath(workspacePath, sessionId)
  let existing: any = {
    id: sessionId,
    codebase_path: codebasePath,  // only set on first write; never overwritten below
    created_at: Date.now(),
    token_totals: { input: 0, output: 0, cache_read: 0, cache_write: 0 }
  }
  if (fs.existsSync(p)) {
    try { existing = JSON.parse(fs.readFileSync(p, "utf-8")) } catch {}
    // codebase_path is NOT updated on subsequent turns — write-once
  }
  existing.token_totals.input += usageDelta.input
  existing.token_totals.output += usageDelta.output
  existing.token_totals.cache_read += usageDelta.cache_read
  existing.token_totals.cache_write += usageDelta.cache_write
  const tmp = p + ".tmp"
  fs.writeFileSync(tmp, JSON.stringify(existing, null, 2), "utf-8")
  fs.renameSync(tmp, p)
}
```

Update the `updateSessionJson` call site to pass `codebasePath`.

- [x] **Step 3: Fix `initSession` to use resolved `codebasePath`**

The current `initSession` call uses a hardcoded `path.join(workspacePath, "codebase")` for the hash. Find and update it to use the resolved `codebasePath` variable:

```typescript
initSession({
  transcriptPath: txPath,
  id: sessionId,
  model: config.model,
  codebaseHash: getCodebaseHash(codebasePath),  // ← use resolved codebasePath
  codebaseDirty: isCodebaseDirty(codebasePath),
})
```

- [x] **Step 4: Call `getCodebaseDirtyFiles` once and reuse**

`git status --porcelain` is called multiple times per turn (once in `isCodebaseDirty`, once in `getCodebaseDirtyFiles`). Call it once and derive both:

```typescript
const dirtyFiles = getCodebaseDirtyFiles(codebasePath)
const codebaseDirty = dirtyFiles.length > 0
const codebaseHash = getCodebaseHash(codebasePath)
```

Use `codebaseDirty` and `codebaseHash` in:
- `initSession` call (first turn)
- `turn_complete` event
- `buildTurnContext`

- [x] **Step 5: Inject system prompt and turn context into `streamText`**

Add imports:
```typescript
import { buildSystemPrompt, buildTurnContext } from "../../system_prompt"
```

Build both before the `streamText` call:

```typescript
const systemPrompt = buildSystemPrompt({ sessionId, workspacePath, codebasePath })

const turnContextMsg = buildTurnContext({
  codebaseHash,
  dirtyFiles,
  timestamp: Date.now(),
})

// Prepend turn context as first user message
;(messages as any[]).unshift({ role: "user", content: turnContextMsg })
```

Pass `system` to `streamText`:

```typescript
const result = streamText({
  model,
  system: systemPrompt,
  messages,
  tools: trackedTools,
  maxSteps: 50,
  abortSignal: ac.signal,
  onStepFinish: async (...) => { ... }
})
```

- [x] **Step 6: Run all tests**

```bash
bun test
```
Expected: all pass

- [x] **Step 7: Build and smoke test**

```bash
bun run build && cp dist/gait ~/develop/gait
```

Verify system prompt and turn context are injected:
```bash
TMP=$(mktemp -d)
cd $TMP
git init && git config user.email 'x@x.com' && git config user.name 'X'
echo '{ "model": "minimax/MiniMax-M3", "allow": ["ls","read","glob"] }' > gait.jsonc
# AGENTS.md not created — verify <agents> block is omitted
gait run --workspace $TMP --codebase /tmp --task "what session are you in and what is the codebase path?" --auto

# Verify session.json has codebase_path
cat $TMP/sessions/s*/session.json | python3 -m json.tool

# Verify each session is on its own branch
git -C $TMP/sessions/s*/ log --oneline

# Roll back one turn
S=$(ls $TMP/sessions | head -1)
git -C $TMP/sessions/$S reset HEAD~1
git -C $TMP/sessions/$S log --oneline  # init commit only
```

- [x] **Step 8: Commit**

```bash
git add src/cli/cmd/run.ts
git commit -m "feat(run): session worktrees, system prompt, turn context, codebase_path persistence"
```

---

## Rollback UX (informational — no code change)

```bash
git -C sessions/sA reset HEAD~1        # undo last turn, keep files
git -C sessions/sA reset --hard HEAD~1 # undo last turn + discard changes
git -C sessions/sA log --oneline       # view session history
```

Future `gait session log <id>` / `gait session rollback <id>` subcommand — out of scope for this plan.
