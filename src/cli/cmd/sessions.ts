import { listActiveSessions } from "../../git"

export async function sessionsCommand(argv: string[]): Promise<void> {
  let workspacePath = process.cwd()

  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--workspace" && argv[i + 1]) {
      workspacePath = argv[++i]!
    }
  }

  const active = listActiveSessions(workspacePath)
  if (active.length === 0) {
    process.stdout.write("No active sessions.\n")
    return
  }
  for (const id of active) {
    process.stdout.write(`${id}\n`)
  }
}
