import { z } from "zod"
import fs from "fs"

const ConfigDefinedTool = z.object({
  name: z.string(),
  description: z.string(),
  cmd: z.string(),
  cwd: z.enum(["codebase", "workspace"]).default("codebase"),
})

const McpServerConfig = z.object({
  name: z.string(),
  cmd: z.string(),
  args: z.array(z.string()).default([]),
  env: z.record(z.string()).default({}),
})

const ConfigSchema = z.object({
  model: z.string(),
  default_agent: z.string().optional(),
  allow: z.array(z.string()).default([]),
  tools: z.array(ConfigDefinedTool).default([]),
  mcpServers: z.array(McpServerConfig).default([]),
  provider: z.record(z.unknown()).default({}),
  compaction_threshold: z.number().min(0.1).max(1.0).default(0.8),
})

export type Config = z.infer<typeof ConfigSchema>
export type ConfigDefinedTool = z.infer<typeof ConfigDefinedTool>
export type McpServerConfig = z.infer<typeof McpServerConfig>

// Strip // line comments and /* block comments */ from JSONC before parsing
function stripComments(jsonc: string): string {
  return jsonc
    .replace(/\/\/[^\n]*/g, "")
    .replace(/\/\*[\s\S]*?\*\//g, "")
}

export function parseConfig(jsonc: string): Config {
  const json = stripComments(jsonc)
  return ConfigSchema.parse(JSON.parse(json))
}

export function loadConfig(workspacePath: string): Config {
  const configPath = `${workspacePath}/gait.jsonc`
  if (!fs.existsSync(configPath)) {
    throw new Error(`No gait.jsonc found at ${configPath}. Run 'gait init' first.`)
  }
  return parseConfig(fs.readFileSync(configPath, "utf-8"))
}
