import fs from "fs"
import path from "path"
import { checkGitInstalled, initWorkspace, addSubmodule, initialCommit } from "../../git"

const DEFAULT_CONFIG = `{
  // gait configuration
  "model": "anthropic/claude-sonnet-4-6",
  // Allow list: tools that run without approval. Default empty = everything gated.
  "allow": [],
  // Tier 1 config-defined tools: shell commands the LLM can call by name
  "tools": []
}
`

export async function initCommand(argv: string[]): Promise<void> {
  const [repoUrl] = argv
  const workspacePath = process.cwd()

  checkGitInstalled()
  initWorkspace(workspacePath)

  // Create directories
  fs.mkdirSync(path.join(workspacePath, "sessions"), { recursive: true })

  // Write .gitignore
  const gitignorePath = path.join(workspacePath, ".gitignore")
  const gitignoreContent = "sessions/*/lock\n.gait-git.lock\n"
  if (!fs.existsSync(gitignorePath)) {
    fs.writeFileSync(gitignorePath, gitignoreContent, "utf-8")
  } else {
    const existing = fs.readFileSync(gitignorePath, "utf-8")
    if (!existing.includes("sessions/*/lock")) {
      fs.appendFileSync(gitignorePath, "\n" + gitignoreContent)
    }
  }

  // Write gait.jsonc if not present
  const configPath = path.join(workspacePath, "gait.jsonc")
  if (!fs.existsSync(configPath)) {
    fs.writeFileSync(configPath, DEFAULT_CONFIG, "utf-8")
  }

  if (repoUrl) {
    addSubmodule(workspacePath, repoUrl)
    initialCommit(workspacePath)
    console.log(`\ngait workspace initialized with codebase at ${repoUrl}`)
    console.log(`\nStart an agent turn:`)
    console.log(`  gait run --task "..."`)
  } else {
    initialCommit(workspacePath)
    console.log(`\ngait workspace initialized`)
    console.log(`\nAdd your codebase as a git submodule:`)
    console.log(`  git submodule add <your-repo-url> codebase`)
    console.log(`\nThen start an agent turn:`)
    console.log(`  gait run --task "..."`)
  }
}
