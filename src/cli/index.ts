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
    default:
      console.log("Usage: ainix <init|run|auth|models>")
      console.log("  init [<repo-url>]       Initialize workspace")
      console.log("  run [options] [files]   Run one agent turn")
      console.log("    --session <id>          Session ID (auto-generated if omitted)")
      console.log("    --task \"...\"          Task instruction for this turn")
      console.log("    --auto                  Auto-approve all permissions")
      console.log("  auth <add|list|remove>  Manage API credentials")
      console.log("  models [provider]       List available models")
      process.exit(cmd ? 1 : 0)
  }
}

main().catch((err) => {
  console.error(err)
  process.exit(1)
})
