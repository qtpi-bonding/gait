import fs from "fs"
import path from "path"
import crypto from "crypto"
import { streamText, type CoreTool } from "ai"
import { loadConfig } from "../../config"
import { getSDK } from "../../provider/provider"
import { getModelContextLength } from "../../provider/models"
import { acquireLock, releaseLock } from "../../lock"
import { commitSession, getCodebaseHash, isCodebaseDirty } from "../../git"
import {
  readEvents, appendEvent, lastEvent, countTurnComplete, lastCompactionIndex, initSession,
  type AnyEvent, type PermissionsEvent, type PermissionsReplyEvent,
  type QuestionsEvent, type TurnCompleteEvent, type CompactionEvent,
  type SubagentSpawnedEvent, type SubagentCompleteEvent,
} from "../../session/transcript"
import { reconstructMessages } from "../../session/context"
import { needsCompaction, compact } from "../../session/compaction"
import { wrapTools, type PendingPermission } from "../../permissions"
import { createBuiltinTools, createConfigTools } from "../../tools/index"
import { Auth } from "../../auth/index"
import { createSpawnSubagentTool, registerSubagentRunner, type SubagentResult } from "../../tools/spawn_subagent"
import { getQuestionsSignal, resetQuestionsSignal } from "../../tools/questions"
import { DualStream } from "../../stream"
import readline from "readline"

interface RunArgs {
  sessionId: string
  task?: string
  auto: boolean
  files: string[]
  workspacePath: string
  codebasePath?: string  // overrides default <workspacePath>/codebase
  isSubagent?: boolean
}

function ensureSessionDir(workspacePath: string, sessionId: string): string {
  const dir = path.join(workspacePath, "sessions", sessionId)
  fs.mkdirSync(dir, { recursive: true })
  return dir
}

function transcriptPath(workspacePath: string, sessionId: string): string {
  return path.join(workspacePath, "sessions", sessionId, "transcript.jsonl")
}

function sessionJsonPath(workspacePath: string, sessionId: string): string {
  return path.join(workspacePath, "sessions", sessionId, "session.json")
}

function updateSessionJson(workspacePath: string, sessionId: string, model: string, usageDelta: { input: number; output: number; cache_read: number; cache_write: number }): void {
  const p = sessionJsonPath(workspacePath, sessionId)
  let existing: any = { id: sessionId, model, created_at: Date.now(), token_totals: { input: 0, output: 0, cache_read: 0, cache_write: 0 } }
  if (fs.existsSync(p)) {
    try { existing = JSON.parse(fs.readFileSync(p, "utf-8")) } catch {}
  }
  existing.token_totals.input += usageDelta.input
  existing.token_totals.output += usageDelta.output
  existing.token_totals.cache_read += usageDelta.cache_read
  existing.token_totals.cache_write += usageDelta.cache_write

  const tmp = p + ".tmp"
  fs.writeFileSync(tmp, JSON.stringify(existing, null, 2), "utf-8")
  fs.renameSync(tmp, p)
}

async function promptLine(question: string): Promise<string> {
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout })
  return new Promise((resolve) => rl.question(question, (ans) => { rl.close(); resolve(ans.trim()) }))
}

async function resolvePermissions(
  pending: PendingPermission[],
  txPath: string,
  codebasePath: string,
  auto: boolean,
  sessionId: string,
): Promise<void> {
  const items: PermissionsEvent["items"] = pending.map((p) => ({
    id: p.id,
    cmd: `${p.name}(${JSON.stringify(p.input)})`,
    reason: p.reason,
  }))
  appendEvent(txPath, { type: "permissions", items, ts: Date.now() })

  if (auto) {
    const replyItems: PermissionsReplyEvent["items"] = []
    for (const p of pending) {
      replyItems.push({ id: p.id, decision: "approve" })
    }
    appendEvent(txPath, { type: "permissions_reply", from: "auto", items: replyItems, ts: Date.now() })
    return
  }

  // Show all requests first
  process.stdout.write("\nPermission requests:\n")
  for (let i = 0; i < pending.length; i++) {
    const p = pending[i]!
    process.stdout.write(`  [${i + 1}] ${p.name}(${JSON.stringify(p.input)})  — ${p.reason}\n`)
  }
  process.stdout.write("\n")

  // Then collect answers
  const replyItems: PermissionsReplyEvent["items"] = []
  for (let i = 0; i < pending.length; i++) {
    const p = pending[i]!
    const answer = await promptLine(`[${i + 1}] y/n > `)
    if (answer === "y" || answer === "yes") {
      replyItems.push({ id: p.id, decision: "approve" })
    } else if (answer === "n" || answer === "no" || answer === "") {
      replyItems.push({ id: p.id, decision: "deny" })
    } else {
      replyItems.push({ id: p.id, decision: "instruct", text: answer })
    }
  }
  appendEvent(txPath, { type: "permissions_reply", from: "user", items: replyItems, ts: Date.now() })
}

async function resolveQuestions(txPath: string, sessionId: string): Promise<void> {
  const events = readEvents(txPath)
  const last = events[events.length - 1]
  if (!last || last.type !== "questions") return

  // Show all questions first
  process.stdout.write("\nQuestions:\n")
  for (let i = 0; i < last.items.length; i++) {
    process.stdout.write(`  [${i + 1}] ${last.items[i]!.content}\n`)
  }
  process.stdout.write("\n")

  // Then collect answers
  const replyItems: Array<{ id: string; content: string }> = []
  for (let i = 0; i < last.items.length; i++) {
    const answer = await promptLine(`[${i + 1}] > `)
    replyItems.push({ id: last.items[i]!.id, content: answer })
  }
  appendEvent(txPath, { type: "questions_reply", from: "user", items: replyItems, ts: Date.now() })
}

// Core turn execution — also used by spawn_subagent
export async function runTurn(args: RunArgs): Promise<SubagentResult | null> {
  const { sessionId, task, auto, files, workspacePath, isSubagent = false } = args
  const codebasePath = args.codebasePath ?? path.join(workspacePath, "codebase")
  const sessionDir = ensureSessionDir(workspacePath, sessionId)
  const txPath = transcriptPath(workspacePath, sessionId)

  // 1. Acquire lock
  acquireLock(sessionDir)

  try {
    const config = loadConfig(workspacePath)
    const providerId = config.model.split("/")[0] ?? ""
    const authEntry = await Auth.get(providerId)
    const apiKey = authEntry?.type === "api" ? authEntry.key : undefined
    const model = getSDK(config.model, apiKey)

    // 1b. Initialize session transcript on first run
    if (!fs.existsSync(txPath) || fs.readFileSync(txPath, "utf-8").trim() === "") {
      initSession({
        transcriptPath: txPath,
        id: sessionId,
        model: config.model,
        codebaseHash: getCodebaseHash(path.join(workspacePath, "codebase")),
        codebaseDirty: isCodebaseDirty(path.join(workspacePath, "codebase")),
      })
    }

    // 2. Handle pending events from last turn
    const lastEvt = lastEvent(txPath)
    if (lastEvt?.type === "questions") {
      if (isSubagent) {
        throw new Error("subagents cannot ask questions in v1")
      }
      await resolveQuestions(txPath, sessionId)
    } else if (lastEvt?.type === "permissions") {
      await resolvePermissions(
        [],
        txPath,
        codebasePath,
        auto,
        sessionId,
      )
    }

    // 3. Append user_turn if --task provided
    if (task) {
      appendEvent(txPath, { type: "user_turn", content: task, ts: Date.now() })
    }

    // 5. Git status
    const codebaseHash = getCodebaseHash(codebasePath)
    const codebaseDirty = isCodebaseDirty(codebasePath)

    // 5b. Token estimation + compaction
    const events = readEvents(txPath)
    const breakpoint = lastCompactionIndex(events)
    const relevantEvents = breakpoint >= 0 ? events.slice(breakpoint) : events
    const contextWindow = (await getModelContextLength(config.model)) ?? 128_000
    if (needsCompaction(relevantEvents, contextWindow, config.compaction_threshold)) {
      process.stderr.write("[gait] compacting session...\n")
      const { summary, usage } = await compact({ model, events: relevantEvents, contextWindow })
      const compactionEvent: CompactionEvent = {
        type: "compaction",
        summary,
        turns_covered: countTurnComplete(relevantEvents),
        from_ts: relevantEvents[0]!.ts,
        to_ts: relevantEvents[relevantEvents.length - 1]!.ts,
        usage,
        ts: Date.now(),
      }
      appendEvent(txPath, compactionEvent)
      commitSession(workspacePath, sessionId, `session(${sessionId}): compaction`)
    }

    // 6. Reconstruct messages
    const messages = reconstructMessages(txPath)
    if (files.length > 0) {
      const fileContents = files.map((f) => {
        const p = path.join(codebasePath, f)
        const content = fs.existsSync(p) ? fs.readFileSync(p, "utf-8") : `(file not found: ${f})`
        return `<file path="${f}">\n${content}\n</file>`
      }).join("\n\n")
      ;(messages as any[]).unshift({ role: "user", content: `Context files:\n\n${fileContents}` })
    }

    // 7. Build tools
    const builtins = createBuiltinTools(codebasePath)
    const configTools = createConfigTools(config.tools, workspacePath, codebasePath)

    for (const name of Object.keys(configTools)) {
      if (name in builtins) {
        throw new Error(`Config tool name "${name}" collides with a built-in tool name.`)
      }
    }

    const allTools = { ...builtins, ...configTools }

    // Register subagent runner
    registerSubagentRunner(async (opts) => {
      const result = await runTurn({
        sessionId: opts.sessionId,
        task: opts.task,
        auto: true,
        files: opts.contextFiles,
        workspacePath: opts.workspacePath,
        isSubagent: true,
      })
      return result ?? { status: "error", result: "subagent returned null" }
    })

    const spawnSubagentTool = createSpawnSubagentTool(sessionId, workspacePath)
    const allToolsWithSubagent = { ...allTools, spawn_subagent: spawnSubagentTool }

    // 8. Stream LLM
    const stream = new DualStream()
    const pendingPermissions: PendingPermission[] = []
    resetQuestionsSignal()

    const ac = new AbortController()

    const wrappedTools = wrapTools(allToolsWithSubagent, config.allow, pendingPermissions, codebasePath)

    // Add event-appending wrappers on top of permission wrappers
    const trackedTools: Record<string, CoreTool> = {}
    for (const [name, t] of Object.entries(wrappedTools)) {
      trackedTools[name] = {
        ...t,
        execute: async (args: any, opts: any) => {
          const isPermGated = !config.allow.includes(name) && name !== "questions" && name !== "spawn_subagent"
          if (isPermGated) {
            return await (t as any).execute(args, opts)
          }
          const id = crypto.randomUUID()
          appendEvent(txPath, { type: "tool_call", id, name, input: args, ts: Date.now() })
          const result = await (t as any).execute(args, opts)
          appendEvent(txPath, { type: "tool_result", id, output: String(result), ts: Date.now() })
          return result
        },
      } as any
    }

    let lastStepText = ""
    let totalUsage = { input: 0, output: 0, cache_read: 0, cache_write: 0 }

    try {
      // No await — streamText returns immediately; streaming happens in the for-await below
      const result = streamText({
        model,
        messages,
        tools: trackedTools,
        maxSteps: 50,
        abortSignal: ac.signal,
        onStepFinish: async ({ text, usage: stepUsage }) => {
          const content = stream.flushMessage()
          if (content) {
            appendEvent(txPath, { type: "assistant_text", content, ts: Date.now() })
            lastStepText = content
          }

          totalUsage.input += stepUsage.promptTokens
          totalUsage.output += stepUsage.completionTokens

          if (getQuestionsSignal() || pendingPermissions.length > 0) {
            ac.abort()
          }
        },
      })

      for await (const delta of result.textStream) {
        stream.writeToken(delta)
      }
    } catch (err: any) {
      if (err?.name === "AbortError") {
        // Expected abort from questions/permissions signal
      } else {
        appendEvent(txPath, { type: "error", message: String(err), phase: "llm", ts: Date.now() })
        commitSession(workspacePath, sessionId, `session(${sessionId}): error`)
        if (isSubagent) return { status: "error", result: String(err) }
        process.exit(1)
      }
    }

    // Check what ended the turn
    const questionsSignal = getQuestionsSignal()

    if (questionsSignal) {
      appendEvent(txPath, { type: "questions", items: questionsSignal.items, ts: Date.now() })
      if (isSubagent) {
        appendEvent(txPath, { type: "error", message: "subagents cannot ask questions in v1", phase: "tool", ts: Date.now() })
        commitSession(workspacePath, sessionId, `session(${sessionId}): error`)
        return { status: "error", result: "subagents cannot ask questions in v1" }
      }
      commitSession(workspacePath, sessionId, `session(${sessionId}): questions`)
      process.exit(0)
    }

    if (pendingPermissions.length > 0) {
      await resolvePermissions(pendingPermissions, txPath, codebasePath, auto, sessionId)
      if (auto) {
        pendingPermissions.length = 0
        return runTurn({ ...args })
      }
      commitSession(workspacePath, sessionId, `session(${sessionId}): permissions`)
      process.exit(0)
    }

    // turn_complete
    const turnN = countTurnComplete(readEvents(txPath)) + 1
    const turnCompleteEvt: TurnCompleteEvent = {
      type: "turn_complete",
      usage: totalUsage,
      codebase_hash: getCodebaseHash(codebasePath),
      codebase_dirty: isCodebaseDirty(codebasePath),
      ts: Date.now(),
    }
    appendEvent(txPath, turnCompleteEvt)
    updateSessionJson(workspacePath, sessionId, config.model, totalUsage)
    commitSession(workspacePath, sessionId, `session(${sessionId}): turn ${turnN}`)

    if (isSubagent) {
      return { status: "done", result: lastStepText }
    }
    return null

  } catch (err: any) {
    appendEvent(txPath, { type: "error", message: String(err), phase: "init", ts: Date.now() })
    try { commitSession(workspacePath, sessionId, `session(${sessionId}): error`) } catch {}
    if (isSubagent) return { status: "error", result: String(err) }
    process.stderr.write(`[gait] error: ${err}\n`)
    process.exit(1)
  } finally {
    releaseLock(sessionDir)
  }
}

// CLI entry for `gait run`
export async function runCommand(argv: string[]): Promise<void> {
  let sessionId: string | undefined
  let task: string | undefined
  let auto = false
  const files: string[] = []
  let workspacePath = process.cwd()
  let codebasePath: string | undefined

  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--session" && argv[i + 1]) {
      sessionId = argv[++i]
    } else if (argv[i] === "--task" && argv[i + 1]) {
      task = argv[++i]
    } else if (argv[i] === "--workspace" && argv[i + 1]) {
      workspacePath = argv[++i]!
    } else if (argv[i] === "--codebase" && argv[i + 1]) {
      codebasePath = argv[++i]!
    } else if (argv[i] === "--auto") {
      auto = true
    } else if (argv[i] && !argv[i]!.startsWith("-")) {
      files.push(argv[i]!)
    }
  }

  if (!sessionId) {
    sessionId = `s${crypto.randomUUID().slice(0, 8)}`
    process.stderr.write(`[gait] starting session ${sessionId}\n`)
  }

  await runTurn({ sessionId, task, auto, files, workspacePath, codebasePath })
}
