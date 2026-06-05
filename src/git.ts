import { execSync } from "child_process"
import path from "path"

function run(cmd: string, cwd: string): string {
  return execSync(cmd, { cwd, encoding: "utf-8" }).trim()
}

export function checkGitInstalled(): void {
  try {
    run("git --version", process.cwd())
  } catch {
    throw new Error("git is not installed or not in PATH. gait requires git.")
  }
}

export function getCodebaseHash(codebasePath: string): string {
  try {
    return run("git rev-parse HEAD", codebasePath)
  } catch {
    return "none"
  }
}

export function isCodebaseDirty(codebasePath: string): boolean {
  try {
    return run("git status --porcelain", codebasePath).length > 0
  } catch {
    return false
  }
}

export function commitSession(workspacePath: string, sessionId: string, message: string): void {
  try {
    run(`git add sessions/${sessionId}/`, workspacePath)
    run(`git commit -m "${message.replace(/"/g, '\\"')}"`, workspacePath)
  } catch (err) {
    throw new Error(`git commit failed: ${err}`)
  }
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
  run("git add -A", workspacePath)
  run('git commit -m "gait: init workspace"', workspacePath)
}
