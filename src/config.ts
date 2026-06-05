import { z } from "zod"
import fs from "fs"

const ConfigDefinedTool = z.object({
  name: z.string(),
  description: z.string(),
  cmd: z.string(),
  cwd: z.enum(["codebase", "workspace"]).default("codebase"),
})

const ConfigSchema = z.object({
  model: z.string(),
  allow: z.array(z.string()).default([]),
  tools: z.array(ConfigDefinedTool).default([]),
  provider: z.record(z.unknown()).default({}),
  compaction_threshold: z.number().min(0.1).max(1.0).default(0.8),
})

export type Config = z.infer<typeof ConfigSchema>
export type ConfigDefinedTool = z.infer<typeof ConfigDefinedTool>

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
