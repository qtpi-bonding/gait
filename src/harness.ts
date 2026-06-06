import path from "path"
import { spawn } from "child_process"
import { acquireLock, releaseLock } from "./lock"
import { appendEvent } from "./session/transcript"
import { commitSession } from "./git"

export type TurnResult =
  | { type: "done"; summary: string }
  | { type: "clarify"; items: Array<{ id: string; content: string; why: string; kind: string; to?: string }> }
  | { type: "inform"; items: Array<{ id: string; content: string; context?: string; to?: string }> }
  | { type: "permissions" }

export interface HarnessBackend {
  afterTurn(sessionId: string, assignedBy: string | undefined, result: TurnResult): Promise<void>
}

function buildSummary(result: TurnResult): string {
  if (result.type === "done") return result.summary
  if (result.type === "clarify") return result.items.map((i) => i.content).join("; ")
  if (result.type === "inform") return result.items.map((i) => i.content).join("; ")
  return "needs permissions approval"
}

function getGaitInvocation(): string[] {
  const argv1 = process.argv[1] ?? ""
  if (argv1.endsWith(".ts") || argv1.endsWith(".js")) {
    return [process.execPath, argv1]
  }
  return [process.execPath]
}

export class NativeHarnessBackend implements HarnessBackend {
  constructor(private workspacePath: string) {}

  async afterTurn(
    sessionId: string,
    assignedBy: string | undefined,
    result: TurnResult
  ): Promise<void> {
    if (assignedBy === undefined) return

    const summary = buildSummary(result)

    const event = {
      type: "agent_update" as const,
      session_id: sessionId,
      status: (result.type === "permissions" ? "done" : result.type) as "done" | "clarify" | "inform",
      summary: summary.slice(0, 500),
      items:
        result.type === "clarify" || result.type === "inform"
          ? result.items
          : undefined,
      ts: Date.now(),
    }

    const dir = path.join(this.workspacePath, "sessions", assignedBy)
    const transcriptPath = path.join(dir, "transcript.jsonl")

    const maxAttempts = 20
    let acquired = false
    for (let attempt = 0; attempt < maxAttempts; attempt++) {
      try {
        acquireLock(dir)
        acquired = true
        break
      } catch {
        await Bun.sleep(500)
      }
    }

    if (!acquired) {
      throw new Error(`Failed to acquire lock on session ${assignedBy} after ${maxAttempts} attempts`)
    }

    try {
      appendEvent(transcriptPath, event)
      commitSession(this.workspacePath, assignedBy, `session(${assignedBy}): agent_update from ${sessionId}`)
    } finally {
      releaseLock(dir)
    }

    const [cmd, ...rest] = getGaitInvocation()
    spawn(
      cmd!,
      [...rest, "run", "--session", assignedBy, "--workspace", this.workspacePath, "--auto"],
      { detached: true, stdio: "ignore" }
    ).unref()
  }
}
