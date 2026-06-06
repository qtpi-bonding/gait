const [, , cmd, ...rest] = process.argv

async function main() {
  switch (cmd) {
    case "init": {
      const { initCommand } = await import("./cmd/init")
      await initCommand(rest)
      break
    }
    case "run": {
      const { runCommand } = await import("./cmd/run")
      await runCommand(rest)
      break
    }
    case "auth": {
      const { authCommand } = await import("./cmd/auth")
      await authCommand(rest)
      break
    }
    case "models": {
      const { modelsCommand } = await import("./cmd/models")
      await modelsCommand(rest)
      break
    }
    case "finish": {
      const { finishCommand } = await import("./cmd/finish")
      await finishCommand(rest)
      break
    }
    case "sessions": {
      const { sessionsCommand } = await import("./cmd/sessions")
      await sessionsCommand(rest)
      break
    }
    default:
      console.log("Usage: gait <init|run|finish|sessions|auth|models>")
      console.log("  init [<repo-url>]       Initialize workspace")
      console.log("  run [options] [files]   Run one agent turn")
      console.log("    --session <id>          Session ID (auto-generated if omitted)")
      console.log("    --task \"...\"          Task instruction for this turn")
      console.log("    --workspace <path>      Workspace dir (default: cwd)")
      console.log("    --codebase <path>       Codebase dir (default: <workspace>/codebase)")
      console.log("    --agent <name>          Agent from agents/{name}.md (persisted to session)")
      console.log("    --system-prompt \"...\"   Inline system prompt (persisted to session)")
      console.log("    --auto                  Auto-approve all permissions")
      console.log("  finish <session-id>     Merge session branch into main (marks as done)")
      console.log("    --workspace <path>      Workspace dir (default: cwd)")
      console.log("  sessions                List active (unmerged) sessions")
      console.log("    --workspace <path>      Workspace dir (default: cwd)")
      console.log("  auth <add|list|remove>  Manage API credentials")
      console.log("  models [provider]       List available models")
      process.exit(cmd ? 1 : 0)
  }
}

main().catch((err) => {
  console.error(err)
  process.exit(1)
})
