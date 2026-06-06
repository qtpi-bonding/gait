import { describe, it, expect, beforeEach, afterEach } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { execSync } from "child_process"
import {
  createSessionWorktree, removeSessionWorktree,
  commitSession, getCodebaseDirtyFiles,
  finishSession, listActiveSessions,
} from "./git"

let tmpDir: string

function initRepo(dir: string): void {
  execSync("git init", { cwd: dir })
  execSync("git config user.email 'test@test.com'", { cwd: dir })
  execSync("git config user.name 'Test'", { cwd: dir })
  fs.writeFileSync(path.join(dir, "gait.jsonc"), "{}")
  execSync("git add gait.jsonc && git commit -m 'init'", { cwd: dir, shell: "/bin/sh" })
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

  it(".git inside worktree is a file not a dir (confirming it is a worktree)", () => {
    createSessionWorktree(tmpDir, "s1234")
    const gitPath = path.join(tmpDir, "sessions", "s1234", ".git")
    expect(fs.existsSync(gitPath)).toBe(true)
    expect(fs.statSync(gitPath).isFile()).toBe(true)
  })

  it("worktree is on branch session/s1234", () => {
    createSessionWorktree(tmpDir, "s1234")
    const branch = execSync("git rev-parse --abbrev-ref HEAD", {
      cwd: path.join(tmpDir, "sessions", "s1234"), encoding: "utf-8",
    }).trim()
    expect(branch).toBe("session/s1234")
  })

  it("creates .gitignore containing 'lock' inside the worktree", () => {
    createSessionWorktree(tmpDir, "s1234")
    const gi = fs.readFileSync(path.join(tmpDir, "sessions", "s1234", ".gitignore"), "utf-8")
    expect(gi).toContain("lock")
  })

  it("makes an initial commit on the session branch", () => {
    createSessionWorktree(tmpDir, "s1234")
    const log = execSync("git log --oneline", {
      cwd: path.join(tmpDir, "sessions", "s1234"), encoding: "utf-8",
    }).trim()
    expect(log.length).toBeGreaterThan(0)
    expect(log).toContain("s1234")
  })

  it("session branch shares history with main (not an orphan)", () => {
    createSessionWorktree(tmpDir, "s1234")
    const mainHash = execSync("git rev-parse HEAD", { cwd: tmpDir, encoding: "utf-8" }).trim()
    const mergeBase = execSync(
      `git merge-base session/s1234 HEAD`,
      { cwd: tmpDir, encoding: "utf-8" }
    ).trim()
    expect(mergeBase).toBe(mainHash)
  })

  it("does not affect the workspace main branch", () => {
    createSessionWorktree(tmpDir, "s1234")
    const branch = execSync("git rev-parse --abbrev-ref HEAD", {
      cwd: tmpDir, encoding: "utf-8",
    }).trim()
    expect(["main", "master"]).toContain(branch)
  })

  it("is idempotent — calling twice does not throw", () => {
    createSessionWorktree(tmpDir, "s1234")
    expect(() => createSessionWorktree(tmpDir, "s1234")).not.toThrow()
  })

  it("idempotent — second call does not create a duplicate worktree entry", () => {
    createSessionWorktree(tmpDir, "s1234")
    createSessionWorktree(tmpDir, "s1234")
    const list = execSync("git worktree list", { cwd: tmpDir, encoding: "utf-8" })
    const matches = list.match(/session\/s1234/g)
    expect(matches).toHaveLength(1)
  })

  it("recovers from stale worktree (dir deleted without git worktree remove)", () => {
    createSessionWorktree(tmpDir, "s1234")
    fs.rmSync(path.join(tmpDir, "sessions", "s1234"), { recursive: true, force: true })
    expect(() => createSessionWorktree(tmpDir, "s1234")).not.toThrow()
    expect(fs.existsSync(path.join(tmpDir, "sessions", "s1234"))).toBe(true)
  })

  it("falls back with warning when sessions/<id>/ already has files (legacy session)", () => {
    const legacyDir = path.join(tmpDir, "sessions", "sLEGACY")
    fs.mkdirSync(legacyDir, { recursive: true })
    fs.writeFileSync(path.join(legacyDir, "transcript.jsonl"), '{"type":"session_start"}\n')
    const stderrChunks: string[] = []
    const origStderr = process.stderr.write.bind(process.stderr)
    process.stderr.write = (chunk: any) => { stderrChunks.push(String(chunk)); return true }
    try {
      expect(() => createSessionWorktree(tmpDir, "sLEGACY")).not.toThrow()
    } finally {
      process.stderr.write = origStderr
    }
    expect(stderrChunks.join("")).toContain("warning")
    // Directory is preserved untouched
    expect(fs.existsSync(path.join(legacyDir, "transcript.jsonl"))).toBe(true)
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
  it("commits files to the session branch (worktree path)", () => {
    createSessionWorktree(tmpDir, "s9999")
    fs.writeFileSync(path.join(tmpDir, "sessions", "s9999", "transcript.jsonl"), '{"ts":1}\n')
    commitSession(tmpDir, "s9999", "session(s9999): turn 1")
    const log = execSync("git log --oneline", {
      cwd: path.join(tmpDir, "sessions", "s9999"), encoding: "utf-8",
    }).trim()
    expect(log).toContain("turn 1")
  })

  it("does not affect the workspace main branch (worktree path)", () => {
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

  it("legacy path: commits from workspace root when sessions/<id>/.git does not exist", () => {
    // Simulate a legacy session: dir exists but no .git file (predates worktrees)
    const legacyDir = path.join(tmpDir, "sessions", "sLEG")
    fs.mkdirSync(legacyDir, { recursive: true })
    fs.writeFileSync(path.join(legacyDir, "transcript.jsonl"), '{"ts":1}\n')
    commitSession(tmpDir, "sLEG", "session(sLEG): legacy turn 1")
    const log = execSync("git log --oneline", { cwd: tmpDir, encoding: "utf-8" }).trim()
    expect(log).toContain("legacy turn 1")
  })
})

describe("finishSession", () => {
  it("merges session branch into main and removes worktree", () => {
    createSessionWorktree(tmpDir, "s1234")
    fs.writeFileSync(path.join(tmpDir, "sessions", "s1234", "transcript.jsonl"), '{"ts":1}\n')
    commitSession(tmpDir, "s1234", "session(s1234): turn 1")

    finishSession(tmpDir, "s1234")

    // Worktree dir is gone
    expect(fs.existsSync(path.join(tmpDir, "sessions", "s1234"))).toBe(false)
    // Branch is now merged into main
    const merged = execSync("git branch --merged main", { cwd: tmpDir, encoding: "utf-8" })
    expect(merged).toContain("session/s1234")
  })

  it("is safe to call when worktree does not exist", () => {
    expect(() => finishSession(tmpDir, "nonexistent")).not.toThrow()
  })
})

describe("listActiveSessions", () => {
  it("returns empty array when no sessions exist", () => {
    expect(listActiveSessions(tmpDir)).toEqual([])
  })

  it("returns session id for an unmerged session branch", () => {
    createSessionWorktree(tmpDir, "s-active")
    const active = listActiveSessions(tmpDir)
    expect(active).toContain("s-active")
  })

  it("excludes finished (merged) sessions", () => {
    createSessionWorktree(tmpDir, "s-done")
    createSessionWorktree(tmpDir, "s-active")
    finishSession(tmpDir, "s-done")
    const active = listActiveSessions(tmpDir)
    expect(active).not.toContain("s-done")
    expect(active).toContain("s-active")
  })

  it("returns multiple active sessions", () => {
    createSessionWorktree(tmpDir, "s-a1")
    createSessionWorktree(tmpDir, "s-a2")
    const active = listActiveSessions(tmpDir)
    expect(active).toContain("s-a1")
    expect(active).toContain("s-a2")
  })
})

describe("getCodebaseDirtyFiles", () => {
  it("returns [] for a clean repo", () => {
    expect(getCodebaseDirtyFiles(tmpDir)).toEqual([])
  })

  it("returns entries for modified tracked files", () => {
    fs.writeFileSync(path.join(tmpDir, "gait.jsonc"), "modified")
    const files = getCodebaseDirtyFiles(tmpDir)
    expect(files.some(f => f.includes("gait.jsonc"))).toBe(true)
  })

  it("returns entries for untracked files", () => {
    fs.writeFileSync(path.join(tmpDir, "new-file.ts"), "hello")
    const files = getCodebaseDirtyFiles(tmpDir)
    expect(files.some(f => f.includes("new-file.ts"))).toBe(true)
  })

  it("returns [] when path is not a git repo (no throw)", () => {
    const notRepo = fs.mkdtempSync(path.join(os.tmpdir(), "not-a-repo-"))
    try {
      expect(getCodebaseDirtyFiles(notRepo)).toEqual([])
    } finally {
      fs.rmSync(notRepo, { recursive: true, force: true })
    }
  })
})
