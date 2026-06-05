import { execSync, execFileSync } from "child_process"
import fs from "fs"
import path from "path"

function run(cmd: string, cwd: string): string {
  return execSync(cmd, { cwd, encoding: "utf-8" }).trim()
}

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

export function getCodebaseHash(codebasePath: string): string {
  try {
    return run("git rev-parse HEAD", codebasePath)
  } catch {
    return "none"
  }
}

export function getCodebaseDirtyFiles(codebasePath: string): string[] {
  try {
    const out = run("git status --porcelain", codebasePath)
    if (!out) return []
    return out.split("\n").filter(Boolean)
  } catch { return [] }
}

export function isCodebaseDirty(codebasePath: string): boolean {
  return getCodebaseDirtyFiles(codebasePath).length > 0
}

// Atomic lock using exclusive-create — avoids TOCTOU of existsSync+write
function withWorkspaceLock(workspacePath: string, fn: () => void): void {
  const lockPath = path.join(workspacePath, ".gait-ws.lock")
  const deadline = Date.now() + 10_000

  while (true) {
    try {
      const fd = fs.openSync(lockPath, "wx")
      fs.writeSync(fd, String(process.pid))
      fs.closeSync(fd)
      break
    } catch (e: any) {
      if (e.code !== "EEXIST") throw e
      try {
        const pid = parseInt(fs.readFileSync(lockPath, "utf-8").trim(), 10)
        if (!isNaN(pid)) {
          try { process.kill(pid, 0) } catch { fs.unlinkSync(lockPath); continue }
        }
      } catch { continue }
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

  if (!fs.existsSync(gitFile) || !fs.statSync(gitFile).isFile()) {
    try {
      run(`git add sessions/${sessionId}/`, workspacePath)
      const status = run("git status --porcelain", workspacePath)
      if (!status) return
      execFileSync("git", ["commit", "-m", message], { cwd: workspacePath, encoding: "utf-8" })
    } catch (err) { throw new Error(`git commit failed (legacy): ${err}`) }
    return
  }

  try {
    const status = run("git status --porcelain", sessionDir)
    if (!status) return
    run("git add -A", sessionDir)
    execFileSync("git", ["commit", "-m", message], { cwd: sessionDir, encoding: "utf-8" })
  } catch (err) { throw new Error(`git commit failed: ${err}`) }
}

// Used by gait init
export function initWorkspace(workspacePath: string): void {
  try {
    run("git init", workspacePath)
  } catch {
    // already a git repo — fine
  }
}

export function addSubmodule(workspacePath: string, repoUrl: string): void {
  run(`git submodule add "${repoUrl}" codebase`, workspacePath)
}

export function initialCommit(workspacePath: string): void {
  const toStage = ["gait.jsonc", ".gitignore"]
  const agentsPath = path.join(workspacePath, "AGENTS.md")
  if (fs.existsSync(agentsPath)) toStage.push("AGENTS.md")
  run(`git add ${toStage.join(" ")}`, workspacePath)
  run('git commit -m "gait: init workspace"', workspacePath)
}
