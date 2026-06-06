import fs from "fs"
import path from "path"
import crypto from "crypto"
import { streamText, type CoreTool } from "ai"
import { loadConfig } from "../../config"
import { getSDK } from "../../provider/provider"
import { getModelContextLength } from "../../provider/models"
import { acquireLock, releaseLock } from "../../lock"
import { commitSession, getCodebaseHash, getCodebaseDirtyFiles, createSessionWorktree } from "../../git"
import { buildSystemPrompt, buildTurnContext } from "../../system_prompt"
import {
  readEvents, appendEvent, lastEvent, countTurnComplete, lastCompactionIndex, initSession,
  type AnyEvent, type PermissionsEvent, type PermissionsReplyEvent,
  type ClarifyEvent, type ClarifyConvergenceEvent, type InformEvent, type InformConvergenceEvent,
  type TurnCompleteEvent, type CompactionEvent,
  type SubagentSpawnedEvent, type SubagentCompleteEvent,
} from "../../session/transcript"
import { reconstructMessages } from "../../session/context"
import { needsCompaction, compact } from "../../session/compaction"
import { wrapTools, type PendingPermission } from "../../permissions"
import { createBuiltinTools, createConfigTools } from "../../tools/index"
import { createMcpTools } from "../../tools/mcp"
import { Auth } from "../../auth/index"
import { createSpawnSubagentTool, registerSubagentRunner, type SubagentResult } from "../../tools/spawn_subagent"
import {
  getClarifySignal, resetClarifySignal,
  getClarifyConvergenceSignal, resetClarifyConvergenceSignal,
} from "../../tools/clarify"
import {
  getInformSignal, resetInformSignal,
  getInformConvergenceSignal, resetInformConvergenceSignal,
} from "../../tools/inform"
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

function transcriptPath(workspacePath: string, sessionId: string): string {
  return path.join(workspacePath, "sessions", sessionId, "transcript.jsonl")
}

function sessionJsonPath(workspacePath: string, sessionId: string): string {
  return path.join(workspacePath, "sessions", sessionId, "session.json")
}

function readCodebasePath(workspacePath: string, sessionId: string): string | undefined {
  const p = sessionJsonPath(workspacePath, sessionId)
  if (!fs.existsSync(p)) return undefined
  try { return JSON.parse(fs.readFileSync(p, "utf-8")).codebase_path } catch { return undefined }
}

function updateSessionJson(
  workspacePath: string, sessionId: string,
  codebasePath: string,
  usageDelta: { input: number; output: number; cache_read: number; cache_write: number }
): void {
  const p = sessionJsonPath(workspacePath, sessionId)
  let existing: any = {
    id: sessionId,
    codebase_path: codebasePath,
    created_at: Date.now(),
    token_totals: { input: 0, output: 0, cache_read: 0, cache_write: 0 }
  }
  if (fs.existsSync(p)) {
    try { existing = JSON.parse(fs.readFileSync(p, "utf-8")) } catch {}
    // codebase_path is write-once — never overwritten on subsequent turns
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

async function resolveClarify(txPath: string, sessionId: string): Promise<void> {
  const events = readEvents(txPath)
  const last = events[events.length - 1]
  if (!last || last.type !== "clarify") return

  // Show all questions first
  process.stdout.write("\nClarifying questions:\n")
  for (let i = 0; i < last.items.length; i++) {
    const item = last.items[i]!
    process.stdout.write(`  [${i + 1}] (${item.kind}) ${item.content}\n`)
    process.stdout.write(`      why: ${item.why}\n`)
  }
  process.stdout.write("\n")

  // Then collect answers
  const replyItems: ClarifyEvent["items"] = []
  for (let i = 0; i < last.items.length; i++) {
    const item = last.items[i]!
    const answer = await promptLine(`[${i + 1}] > `)
    replyItems.push({ id: item.id, content: answer, why: item.why, kind: item.kind })
  }
  appendEvent(txPath, { type: "clarify_reply", items: replyItems, ts: Date.now() })
}

async function resolveInform(txPath: string, sessionId: string): Promise<void> {
  const events = readEvents(txPath)
  const last = events[events.length - 1]
  if (!last || last.type !== "inform") return

  // Show all inform items first
  process.stdout.write("\nInformation shared with you:\n")
  for (let i = 0; i < last.items.length; i++) {
    const item = last.items[i]!
    process.stdout.write(`  [${i + 1}] ${item.content}\n`)
    if (item.context) {
      process.stdout.write(`      context: ${item.context}\n`)
    }
  }
  process.stdout.write("\n")

  // The LLM explains in its next turn as assistant_text — we don't write an
  // inform_reply event from human input. Just wait for the human to be ready.
  await promptLine("[press enter when ready to hear the explanation]: ")
}

// Core turn execution — also used by spawn_subagent
export async function runTurn(args: RunArgs): Promise<SubagentResult | null> {
  const { sessionId, task, auto, files, workspacePath, isSubagent = false } = args
  const codebasePath = args.codebasePath
    ?? readCodebasePath(workspacePath, sessionId)
    ?? path.join(workspacePath, "codebase")

  createSessionWorktree(workspacePath, sessionId)
  const sessionDir = path.join(workspacePath, "sessions", sessionId)
  const txPath = transcriptPath(workspacePath, sessionId)

  // 1. Acquire lock
  acquireLock(sessionDir)

  let closeMcp: () => Promise<void> = async () => {}

  try {
    const config = loadConfig(workspacePath)
    const providerId = config.model.split("/")[0] ?? ""
    const authEntry = await Auth.get(providerId)
    const apiKey = authEntry?.type === "api" ? authEntry.key : undefined
    const model = getSDK(config.model, apiKey)

    // 1b. Initialize session transcript on first run
    const dirtyFiles = getCodebaseDirtyFiles(codebasePath)
    const codebaseDirty = dirtyFiles.length > 0
    const codebaseHash = getCodebaseHash(codebasePath)

    if (!fs.existsSync(txPath) || fs.readFileSync(txPath, "utf-8").trim() === "") {
      initSession({
        transcriptPath: txPath,
        id: sessionId,
        model: config.model,
        codebaseHash,
        codebaseDirty,
      })
    }

    // 2. Handle pending events from last turn
    const lastEvt = lastEvent(txPath)
    if (lastEvt?.type === "clarify") {
      if (isSubagent) {
        throw new Error("subagents cannot ask clarifying questions in v1")
      }
      await resolveClarify(txPath, sessionId)
    } else if (lastEvt?.type === "inform") {
      if (isSubagent) {
        throw new Error("subagents cannot share information in v1")
      }
      await resolveInform(txPath, sessionId)
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

    // Prepend turn context (volatile: timestamp + git state) as first user message
    const turnContextMsg = buildTurnContext({ codebaseHash, dirtyFiles, timestamp: Date.now() })
    ;(messages as any[]).unshift({ role: "user", content: turnContextMsg })

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
    const { tools: mcpTools, close: closeMcpFn } = await createMcpTools(config.mcpServers)
    closeMcp = closeMcpFn

    for (const name of Object.keys(configTools)) {
      if (name in builtins) {
        throw new Error(`Config tool name "${name}" collides with a built-in tool name.`)
      }
    }
    for (const name of Object.keys(mcpTools)) {
      if (name in builtins || name in configTools) {
        throw new Error(`MCP tool name "${name}" collides with an existing tool name.`)
      }
    }

    const allTools = { ...builtins, ...configTools, ...mcpTools }

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
    const allToolsWithSubagent = isSubagent
      ? allTools
      : { ...allTools, spawn_subagent: spawnSubagentTool }

    // 8. Stream LLM
    const stream = new DualStream()
    const pendingPermissions: PendingPermission[] = []
    resetClarifySignal()
    resetClarifyConvergenceSignal()
    resetInformSignal()
    resetInformConvergenceSignal()

    const ac = new AbortController()

    const wrappedTools = wrapTools(allToolsWithSubagent, config.allow, pendingPermissions, codebasePath)

    // Add event-appending wrappers on top of permission wrappers
    const trackedTools: Record<string, CoreTool> = {}
    for (const [name, t] of Object.entries(wrappedTools)) {
      trackedTools[name] = {
        ...t,
        execute: async (args: any, opts: any) => {
          const isPermGated = !config.allow.includes(name) && name !== "clarify" && name !== "clarify_convergence" && name !== "inform" && name !== "inform_convergence" && name !== "spawn_subagent"
          if (isPermGated) {
            return await (t as any).execute(args, opts)
          }
          const id = crypto.randomUUID()
          appendEvent(txPath, { type: "tool_call", id, name, input: args, ts: Date.now() })
          const result = await (t as any).execute(args, opts)
          const output = typeof result === "string" ? result : JSON.stringify(result)
          appendEvent(txPath, { type: "tool_result", id, output, ts: Date.now() })
          return result
        },
      } as any
    }

    let lastStepText = ""
    let totalUsage = { input: 0, output: 0, cache_read: 0, cache_write: 0 }

    try {
      const systemPrompt = buildSystemPrompt({ sessionId, workspacePath, codebasePath })

      // No await — streamText returns immediately; streaming happens in the for-await below
      const result = streamText({
        model,
        system: systemPrompt,
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

          if (getClarifySignal() || getClarifyConvergenceSignal() || getInformSignal() || getInformConvergenceSignal() || pendingPermissions.length > 0) {
            ac.abort()
          }
        },
      })

      for await (const delta of result.textStream) {
        stream.writeToken(delta)
      }
    } catch (err: any) {
      if (err?.name === "AbortError") {
        // Expected abort from clarify/inform/permissions signal
      } else {
        appendEvent(txPath, { type: "error", message: String(err), phase: "llm", ts: Date.now() })
        commitSession(workspacePath, sessionId, `session(${sessionId}): error`)
        if (isSubagent) return { status: "error", result: String(err) }
        process.exit(1)
      }
    }

    // Check what ended the turn
    const clarifySignal = getClarifySignal()
    const informSignal = getInformSignal()
    const clarifyConvergenceSignal = getClarifyConvergenceSignal()
    const informConvergenceSignal = getInformConvergenceSignal()

    if (clarifySignal) {
      appendEvent(txPath, { type: "clarify", items: clarifySignal.items, ts: Date.now() })
      if (isSubagent) {
        commitSession(workspacePath, sessionId, `session(${sessionId}): clarify`)
        return { status: "needs_clarification", result: JSON.stringify(clarifySignal.items) }
      }
      commitSession(workspacePath, sessionId, `session(${sessionId}): questions`)
      process.exit(0)
    }

    if (informSignal) {
      appendEvent(txPath, { type: "inform", items: informSignal.items, ts: Date.now() })
      if (isSubagent) {
        commitSession(workspacePath, sessionId, `session(${sessionId}): inform`)
        return { status: "needs_inform", result: JSON.stringify(informSignal.items) }
      }
      commitSession(workspacePath, sessionId, `session(${sessionId}): inform`)
      process.exit(0)
    }

    if (clarifyConvergenceSignal) {
      if (isSubagent) {
        appendEvent(txPath, { type: "clarify_convergence", items: clarifyConvergenceSignal.items, ts: Date.now() })
        commitSession(workspacePath, sessionId, `session(${sessionId}): clarify`)
        return { status: "needs_clarification", result: JSON.stringify(clarifyConvergenceSignal.items) }
      }
      const convItems: ClarifyConvergenceEvent["items"] = []
      for (const item of clarifyConvergenceSignal.items) {
        process.stdout.write(`\nUnderstanding [${item.id}]: ${item.proposal}\n`)
        const input = await promptLine(`Correct? [yes / type correction]: `)
        const trimmed = input.trim()
        const consistent = trimmed.toLowerCase() === "yes" || trimmed.toLowerCase() === "y"
        convItems.push({ id: item.id, proposal: item.proposal, response: trimmed, consistent })
      }
      appendEvent(txPath, { type: "clarify_convergence", items: convItems, ts: Date.now() })
      commitSession(workspacePath, sessionId, `session(${sessionId}): clarify_convergence`)
      process.exit(0)
    }

    if (informConvergenceSignal) {
      if (isSubagent) {
        appendEvent(txPath, { type: "inform_convergence", items: informConvergenceSignal.items, ts: Date.now() })
        commitSession(workspacePath, sessionId, `session(${sessionId}): inform`)
        return { status: "needs_inform", result: JSON.stringify(informConvergenceSignal.items) }
      }

      // Look up most recent proposal per id from prior inform_convergence events
      // (needed when the LLM calls without a proposal — second call shape)
      const priorEvents = readEvents(txPath)
      const lastProposalById = new Map<string, string>()
      for (const e of priorEvents) {
        if (e.type === "inform_convergence") {
          for (const it of e.items) {
            lastProposalById.set(it.id, it.proposal)
          }
        }
      }

      const convItems: InformConvergenceEvent["items"] = []
      for (const item of informConvergenceSignal.items) {
        if (item.prompt !== undefined) {
          process.stdout.write(`\n[${item.id}] ${item.prompt}\n`)
          const paraphrase = await promptLine(`Your understanding: `)
          convItems.push({ id: item.id, proposal: paraphrase.trim(), consistent: false })
        } else {
          const proposal = lastProposalById.get(item.id) ?? ""
          convItems.push({
            id: item.id,
            proposal,
            response: item.response,
            consistent: item.consistent ?? false,
          })
        }
      }
      appendEvent(txPath, { type: "inform_convergence", items: convItems, ts: Date.now() })
      commitSession(workspacePath, sessionId, `session(${sessionId}): inform_convergence`)
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
      codebase_hash: codebaseHash,
      codebase_dirty: codebaseDirty,
      ts: Date.now(),
    }
    appendEvent(txPath, turnCompleteEvt)
    updateSessionJson(workspacePath, sessionId, codebasePath, totalUsage)
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
    await closeMcp()
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
