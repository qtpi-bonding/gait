// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { createAmazonBedrock } from "@ai-sdk/amazon-bedrock"
import { createAnthropic } from "@ai-sdk/anthropic"
import { createAzure } from "@ai-sdk/azure"
import { createGoogleGenerativeAI } from "@ai-sdk/google"
import { createVertex } from "@ai-sdk/google-vertex"
import { createOpenAI } from "@ai-sdk/openai"
import { createOpenAICompatible } from "@ai-sdk/openai-compatible"
import { createXai } from "@ai-sdk/xai"
import { createMistral } from "@ai-sdk/mistral"
import { createGroq } from "@ai-sdk/groq"
import { createDeepInfra } from "@ai-sdk/deepinfra"
import { createCerebras } from "@ai-sdk/cerebras"
import { createCohere } from "@ai-sdk/cohere"
import { createTogetherAI } from "@ai-sdk/togetherai"
import { createPerplexity } from "@ai-sdk/perplexity"
import { createOpenRouter } from "@openrouter/ai-sdk-provider"
import type { LanguageModel } from "ai"

// BUNDLED_PROVIDERS maps npm package names to factory functions
// Ported from OpenCode, includes all installed @ai-sdk/* packages
export const BUNDLED_PROVIDERS: Record<string, (options: any) => LanguageModel> = {
  "@ai-sdk/amazon-bedrock": createAmazonBedrock,
  "@ai-sdk/anthropic": createAnthropic,
  "@ai-sdk/azure": createAzure,
  "@ai-sdk/google": createGoogleGenerativeAI,
  "@ai-sdk/google-vertex": createVertex,
  "@ai-sdk/openai": createOpenAI,
  "@ai-sdk/openai-compatible": createOpenAICompatible,
  "@openrouter/ai-sdk-provider": createOpenRouter,
  "@ai-sdk/xai": createXai,
  "@ai-sdk/mistral": createMistral,
  "@ai-sdk/groq": createGroq,
  "@ai-sdk/deepinfra": createDeepInfra,
  "@ai-sdk/cerebras": createCerebras,
  "@ai-sdk/cohere": createCohere,
  "@ai-sdk/togetherai": createTogetherAI,
  "@ai-sdk/perplexity": createPerplexity,
}

// Map provider ID to factory function
// Each factory takes (apiKey, model) and returns a LanguageModel
const CUSTOM_LOADERS: Record<string, (apiKey: string, model: string) => LanguageModel> = {
  anthropic: (apiKey, model) => createAnthropic({ apiKey })(model),
  minimax: (apiKey, model) => createAnthropic({ apiKey, baseURL: "https://api.minimax.io/anthropic/v1" })(model),
  openai: (apiKey, model) => createOpenAI({ apiKey })(model),
  google: (apiKey, model) => createGoogleGenerativeAI({ apiKey })(model),
  "google-vertex": (apiKey, model) => createVertex({ apiKey })(model),
  groq: (apiKey, model) => createGroq({ apiKey })(model),
  mistral: (apiKey, model) => createMistral({ apiKey })(model),
  cohere: (apiKey, model) => createCohere({ apiKey })(model),
  deepinfra: (apiKey, model) => createDeepInfra({ apiKey })(model),
  cerebras: (apiKey, model) => createCerebras({ apiKey })(model),
  xai: (apiKey, model) => createXai({ apiKey })(model),
  togetherai: (apiKey, model) => createTogetherAI({ apiKey })(model),
  perplexity: (apiKey, model) => createPerplexity({ apiKey })(model),
  "amazon-bedrock": (apiKey, model) => createAmazonBedrock({ apiKey })(model),
  azure: (apiKey, model) => createAzure({ apiKey })(model),
  "openai-compatible": (apiKey, model) => createOpenAICompatible({ apiKey })(model),
  openrouter: (apiKey, model) => createOpenRouter({ apiKey })(model),
}

export function getSDK(modelString: string, apiKey?: string): LanguageModel {
  const [providerId, ...rest] = modelString.split("/")
  if (!providerId || rest.length === 0) {
    throw new Error(`Invalid model format "${modelString}". Use "provider/model-id".`)
  }
  const modelId = rest.join("/")

  const loader = CUSTOM_LOADERS[providerId]
  if (!loader) {
    throw new Error(
      `Unknown provider "${providerId}". Available providers: ${Object.keys(CUSTOM_LOADERS).join(", ")}`
    )
  }

  // Build env var name: provider-name → PROVIDER_NAME_API_KEY
  const envVarName = `${providerId.toUpperCase().replace(/-/g, "_")}_API_KEY`
  const key = apiKey ?? process.env[envVarName] ?? ""

  return loader(key, modelId)
}
