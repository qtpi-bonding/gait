import type { CoreMessage, CoreAssistantMessage, CoreToolMessage } from "ai"
import { readEvents, lastCompactionIndex, type AnyEvent } from "./transcript"

export function reconstructMessages(transcriptPath: string): CoreMessage[] {
  const allEvents = readEvents(transcriptPath)
  if (allEvents.length === 0) return []

  // Find the last compaction breakpoint
  const breakpoint = lastCompactionIndex(allEvents)
  const events = breakpoint >= 0 ? allEvents.slice(breakpoint) : allEvents

  const messages: CoreMessage[] = []

  let i = 0
  while (i < events.length) {
    const e = events[i]!

    if (e.type === "compaction") {
      // Inject summary as context
      messages.push({ role: "user", content: `[Session history summary: ${e.summary}]` })
      i++
      continue
    }

    if (e.type === "session_start") { i++; continue }
    if (e.type === "subagent_spawned" || e.type === "subagent_complete") { i++; continue }

    if (e.type === "user_turn") {
      messages.push({ role: "user", content: e.content })
      i++
      continue
    }

    if (e.type === "assistant_text") {
      messages.push({ role: "assistant", content: e.content })
      i++
      continue
    }

    if (e.type === "tool_call") {
      // Gather all consecutive tool_call events (parallel tool calls in one step)
      const callBatch: Array<{ type: "tool_call"; id: string; name: string; input: Record<string, unknown>; ts: number }> = []
      while (i < events.length && events[i]!.type === "tool_call") {
        callBatch.push(events[i] as { type: "tool_call"; id: string; name: string; input: Record<string, unknown>; ts: number })
        i++
      }
      // Gather consecutive tool_result events for this batch
      const resultBatch: Array<{ id: string; output: string }> = []
      while (i < events.length && events[i]!.type === "tool_result") {
        const r = events[i] as { type: "tool_result"; id: string; output: string; ts: number }
        resultBatch.push({ id: r.id, output: r.output })
        i++
      }

      const assistantContent: CoreAssistantMessage["content"] = callBatch.map((c) => ({
        type: "tool-call" as const,
        toolCallId: c.id,
        toolName: c.name,
        args: c.input,
      }))
      messages.push({ role: "assistant", content: assistantContent })

      if (resultBatch.length > 0) {
        const toolContent: CoreToolMessage["content"] = resultBatch.map((r) => ({
          type: "tool-result" as const,
          toolCallId: r.id,
          result: r.output,
        }))
        messages.push({ role: "tool", content: toolContent })
      }
      continue
    }

    if (e.type === "tool_result") {
      // Orphan tool_result (paired call already handled) — skip
      i++
      continue
    }

    if (e.type === "clarify") {
      const items = e.items.map((q) => {
        const lines = [`clarify (${q.kind}): ${q.content}`, `why: ${q.why}`]
        if (q.links && q.links.length > 0) {
          const linkText = q.links.map((l) => `${l.relation} ${l.target}`).join(", ")
          lines.push(`links: ${linkText}`)
        }
        return lines.join("\n")
      })
      messages.push({ role: "assistant", content: items.join("\n\n") })
      i++
      continue
    }

    if (e.type === "clarify_reply") {
      const replyText = e.items.map((q) => {
        const base = `${q.id}: ${q.content}`
        return q.reasoning ? `${base}\nreason: ${q.reasoning}` : base
      }).join("\n")
      messages.push({ role: "user", content: replyText })
      i++
      continue
    }

    if (e.type === "clarify_convergence") {
      for (const item of e.items) {
        messages.push({ role: "assistant", content: `[proposal] ${item.proposal}` })
        if (item.response !== undefined) {
          messages.push({ role: "user", content: `[response] ${item.response}` })
        }
      }
      i++
      continue
    }

    if (e.type === "inform") {
      const items = e.items.map((q) => {
        const base = `inform: ${q.content}`
        return q.context ? `${base}\ncontext: ${q.context}` : base
      })
      messages.push({ role: "assistant", content: items.join("\n\n") })
      i++
      continue
    }

    if (e.type === "inform_reply") {
      const replyText = e.items.map((q) => `${q.id}: ${q.content}`).join("\n")
      messages.push({ role: "assistant", content: replyText })
      i++
      continue
    }

    if (e.type === "inform_convergence") {
      for (const item of e.items) {
        messages.push({ role: "user", content: `[understanding] ${item.proposal}` })
        if (item.response !== undefined) {
          messages.push({ role: "assistant", content: `[correction] ${item.response}` })
        }
      }
      i++
      continue
    }

    if (e.type === "permissions") {
      const permText = e.items.map((p) => `${p.id}: ${p.cmd} (${p.reason})`).join("\n")
      messages.push({ role: "assistant", content: `permission request:\n${permText}` })
      i++
      continue
    }

    if (e.type === "permissions_reply") {
      const lines = e.items.map((p) => {
        if (p.decision === "instruct") return `${p.id}: instruct — ${p.text ?? ""}`
        return `${p.id}: ${p.decision}`
      })
      messages.push({ role: "user", content: `[from: ${e.from}] ${lines.join(", ")}` })
      i++
      continue
    }

    if (e.type === "command_result") {
      // Fold into preceding permissions_reply — append to last user message if it's there
      const last = messages[messages.length - 1]
      const cmdOut = `\ncommand output (exit ${e.exit_code}):\n${e.output}`
      if (last && last.role === "user" && typeof last.content === "string") {
        last.content += cmdOut
      } else {
        messages.push({ role: "user", content: cmdOut })
      }
      i++
      continue
    }

    if (e.type === "error") {
      messages.push({ role: "user", content: `error during ${e.phase}: ${e.message}` })
      i++
      continue
    }

    if (e.type === "turn_complete") { i++; continue }

    i++
  }

  return messages
}
