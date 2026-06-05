import { z } from "zod"
import fs from "fs"

const base = z.object({ ts: z.number() })

export const SessionStartEvent = base.extend({
  type: z.literal("session_start"),
  schema_version: z.literal(1),
  id: z.string(),
  model: z.string(),
  codebase_hash: z.string(),
  codebase_dirty: z.boolean(),
})

export const UserTurnEvent = base.extend({
  type: z.literal("user_turn"),
  content: z.string(),
})

export const AssistantTextEvent = base.extend({
  type: z.literal("assistant_text"),
  content: z.string(),
})

export const ToolCallEvent = base.extend({
  type: z.literal("tool_call"),
  id: z.string(),
  name: z.string(),
  input: z.record(z.unknown()),
})

export const ToolResultEvent = base.extend({
  type: z.literal("tool_result"),
  id: z.string(),
  output: z.string(),
})

export const CommandResultEvent = base.extend({
  type: z.literal("command_result"),
  call_id: z.string(),
  cmd: z.string(),
  output: z.string(),
  exit_code: z.number(),
  duration_ms: z.number(),
  timed_out: z.boolean(),
})

export const ErrorEvent = base.extend({
  type: z.literal("error"),
  message: z.string(),
  phase: z.enum(["llm", "tool", "git", "init"]),
})

export const QuestionItem = z.object({ id: z.string(), content: z.string() })
export const QuestionsEvent = base.extend({
  type: z.literal("questions"),
  items: z.array(QuestionItem),
})

export const QuestionsReplyEvent = base.extend({
  type: z.literal("questions_reply"),
  from: z.string(), // "user" | "session:<id>" | "auto"
  items: z.array(QuestionItem),
})

export const PermissionItem = z.object({ id: z.string(), cmd: z.string(), reason: z.string() })
export const PermissionsEvent = base.extend({
  type: z.literal("permissions"),
  items: z.array(PermissionItem),
})

export const PermissionsReplyItem = z.object({
  id: z.string(),
  decision: z.enum(["approve", "deny", "instruct"]),
  text: z.string().optional(),
})
export const PermissionsReplyEvent = base.extend({
  type: z.literal("permissions_reply"),
  from: z.string(),
  items: z.array(PermissionsReplyItem),
})

export const SubagentSpawnedEvent = base.extend({
  type: z.literal("subagent_spawned"),
  session_id: z.string(),
  task: z.string(),
})

export const SubagentCompleteEvent = base.extend({
  type: z.literal("subagent_complete"),
  session_id: z.string(),
  status: z.enum(["done", "error"]),
  result: z.string(),
})

export const TurnCompleteEvent = base.extend({
  type: z.literal("turn_complete"),
  usage: z.object({
    input: z.number(),
    output: z.number(),
    cache_read: z.number(),
    cache_write: z.number(),
  }),
  codebase_hash: z.string(),
  codebase_dirty: z.boolean(),
})

export const CompactionEvent = base.extend({
  type: z.literal("compaction"),
  summary: z.string(),
  turns_covered: z.number(),
  from_ts: z.number(),
  to_ts: z.number(),
  usage: z.object({ input: z.number(), output: z.number() }),
})

export const AnyEventSchema = z.discriminatedUnion("type", [
  SessionStartEvent,
  UserTurnEvent,
  AssistantTextEvent,
  ToolCallEvent,
  ToolResultEvent,
  CommandResultEvent,
  ErrorEvent,
  QuestionsEvent,
  QuestionsReplyEvent,
  PermissionsEvent,
  PermissionsReplyEvent,
  SubagentSpawnedEvent,
  SubagentCompleteEvent,
  TurnCompleteEvent,
  CompactionEvent,
])

export type SessionStartEvent = z.infer<typeof SessionStartEvent>
export type UserTurnEvent = z.infer<typeof UserTurnEvent>
export type AssistantTextEvent = z.infer<typeof AssistantTextEvent>
export type ToolCallEvent = z.infer<typeof ToolCallEvent>
export type ToolResultEvent = z.infer<typeof ToolResultEvent>
export type CommandResultEvent = z.infer<typeof CommandResultEvent>
export type ErrorEvent = z.infer<typeof ErrorEvent>
export type QuestionsEvent = z.infer<typeof QuestionsEvent>
export type QuestionsReplyEvent = z.infer<typeof QuestionsReplyEvent>
export type PermissionsEvent = z.infer<typeof PermissionsEvent>
export type PermissionsReplyEvent = z.infer<typeof PermissionsReplyEvent>
export type SubagentSpawnedEvent = z.infer<typeof SubagentSpawnedEvent>
export type SubagentCompleteEvent = z.infer<typeof SubagentCompleteEvent>
export type TurnCompleteEvent = z.infer<typeof TurnCompleteEvent>
export type CompactionEvent = z.infer<typeof CompactionEvent>
export type AnyEvent = z.infer<typeof AnyEventSchema>

export function parseEvent(raw: unknown): AnyEvent {
  return AnyEventSchema.parse(raw)
}

// ── Transcript file helpers ──────────────────────────────────────────────────

export function appendEvent(transcriptPath: string, event: AnyEvent): void {
  fs.appendFileSync(transcriptPath, JSON.stringify(event) + "\n", "utf-8")
}

export function readEvents(transcriptPath: string): AnyEvent[] {
  if (!fs.existsSync(transcriptPath)) return []
  return fs.readFileSync(transcriptPath, "utf-8")
    .split("\n")
    .filter(Boolean)
    .map((line) => AnyEventSchema.parse(JSON.parse(line)))
}

export function lastEvent(transcriptPath: string): AnyEvent | null {
  const events = readEvents(transcriptPath)
  return events[events.length - 1] ?? null
}

export function countTurnComplete(events: AnyEvent[]): number {
  return events.filter((e) => e.type === "turn_complete").length
}

export function lastCompactionIndex(events: AnyEvent[]): number {
  for (let i = events.length - 1; i >= 0; i--) {
    if (events[i]!.type === "compaction") return i
  }
  return -1
}

// Initialize a new session transcript with session_start event
export function initSession(opts: {
  transcriptPath: string
  id: string
  model: string
  codebaseHash: string
  codebaseDirty: boolean
}): void {
  const event: SessionStartEvent = {
    type: "session_start",
    schema_version: 1,
    id: opts.id,
    model: opts.model,
    codebase_hash: opts.codebaseHash,
    codebase_dirty: opts.codebaseDirty,
    ts: Date.now(),
  }
  appendEvent(opts.transcriptPath, event)
}
