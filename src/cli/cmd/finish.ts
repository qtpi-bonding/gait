import { finishSession } from "../../git"

export async function finishCommand(argv: string[]): Promise<void> {
  let sessionId: string | undefined
  let workspacePath = process.cwd()

  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--workspace" && argv[i + 1]) {
      workspacePath = argv[++i]!
    } else if (argv[i] && !argv[i]!.startsWith("-")) {
      sessionId = argv[i]!
    }
  }

  if (!sessionId) {
    process.stderr.write("Usage: gait finish <session-id> [--workspace <path>]\n")
    process.exit(1)
  }

  finishSession(workspacePath, sessionId)
  process.stderr.write(`[gait] session ${sessionId} merged into main\n`)
}
