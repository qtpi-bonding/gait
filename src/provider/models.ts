import fs from "fs"
import path from "path"
import os from "os"

const MODELS_URL = "https://models.dev/api.json"
const CACHE_DIR = path.join(os.homedir(), ".cache", "gait")
const CACHE_FILE = path.join(CACHE_DIR, "models.json")
const CACHE_TTL_MS = 60 * 60 * 1000 // 1 hour

export interface ModelInfo {
  id: string
  name: string
  provider: string
  contextLength?: number
  [key: string]: unknown
}

async function fetchModels(): Promise<ModelInfo[]> {
  const res = await fetch(MODELS_URL)
  if (!res.ok) throw new Error(`models.dev returned ${res.status}`)
  const data = await res.json() as unknown
  // models.dev returns an array of model objects
  if (!Array.isArray(data)) throw new Error("Unexpected models.dev response shape")
  return data as ModelInfo[]
}

export async function getModels(): Promise<ModelInfo[]> {
  fs.mkdirSync(CACHE_DIR, { recursive: true })

  // Check cache freshness
  if (fs.existsSync(CACHE_FILE)) {
    const stat = fs.statSync(CACHE_FILE)
    const age = Date.now() - stat.mtimeMs
    if (age < CACHE_TTL_MS) {
      return JSON.parse(fs.readFileSync(CACHE_FILE, "utf-8")) as ModelInfo[]
    }
  }

  // Fetch fresh
  try {
    const models = await fetchModels()
    fs.writeFileSync(CACHE_FILE, JSON.stringify(models, null, 2), "utf-8")
    return models
  } catch (err) {
    // Offline fallback: use stale cache
    if (fs.existsSync(CACHE_FILE)) {
      process.stderr.write(`[warn] models.dev unreachable, using cached model list\n`)
      return JSON.parse(fs.readFileSync(CACHE_FILE, "utf-8")) as ModelInfo[]
    }
    throw new Error(
      `Cannot fetch model list and no cache exists. Specify "model" directly in gait.jsonc. (${err})`
    )
  }
}

export async function getModelContextLength(modelId: string): Promise<number | null> {
  try {
    const models = await getModels()
    const [provider, ...rest] = modelId.split("/")
    const name = rest.join("/")
    const found = models.find(
      (m) => m.id === name || m.id === modelId ||
        (m.provider === provider && (m.id === name || m.name === name))
    )
    return typeof found?.contextLength === "number" ? found.contextLength : null
  } catch {
    return null
  }
}
