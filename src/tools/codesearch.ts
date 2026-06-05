// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { tool } from "ai"
import { z } from "zod"

const API_BASE_URL = "https://mcp.exa.ai"
const API_ENDPOINT = "/mcp"
const DEFAULT_TIMEOUT = 30_000

interface McpCodeRequest {
  jsonrpc: string
  id: number
  method: string
  params: {
    name: string
    arguments: {
      query: string
      tokensNum: number
    }
  }
}

interface McpCodeResponse {
  jsonrpc: string
  result: {
    content: Array<{
      type: string
      text: string
    }>
  }
}

export function createCodesearchTool(_codebasePath: string) {
  return tool({
    description:
      "Search for code examples, API documentation, and SDK usage on the web via Exa. Use for finding how to use specific libraries or frameworks.",
    parameters: z.object({
      query: z
        .string()
        .describe(
          "Search query for APIs, libraries, and SDKs. E.g. 'React useState hook examples', 'Python pandas dataframe filtering', 'Express.js middleware'",
        ),
      tokensNum: z
        .number()
        .min(1000)
        .max(50000)
        .default(5000)
        .describe(
          "Number of tokens to return (1000-50000). Default is 5000. Use lower values for focused queries, higher for comprehensive docs.",
        ),
    }),
    execute: async ({ query, tokensNum = 5000 }) => {
      const codeRequest: McpCodeRequest = {
        jsonrpc: "2.0",
        id: 1,
        method: "tools/call",
        params: {
          name: "get_code_context_exa",
          arguments: {
            query,
            tokensNum,
          },
        },
      }

      const controller = new AbortController()
      const timeout = setTimeout(() => controller.abort(), DEFAULT_TIMEOUT)

      try {
        const response = await fetch(`${API_BASE_URL}${API_ENDPOINT}`, {
          method: "POST",
          headers: {
            accept: "application/json, text/event-stream",
            "content-type": "application/json",
          },
          body: JSON.stringify(codeRequest),
          signal: controller.signal,
        })

        clearTimeout(timeout)

        if (!response.ok) {
          const errorText = await response.text()
          throw new Error(`Code search error (${response.status}): ${errorText}`)
        }

        const responseText = await response.text()

        // Parse SSE response
        for (const line of responseText.split("\n")) {
          if (line.startsWith("data: ")) {
            const data: McpCodeResponse = JSON.parse(line.substring(6))
            if (data.result?.content?.length > 0) {
              return data.result.content[0]!.text
            }
          }
        }

        return "No code snippets or documentation found. Try a different query, be more specific about the library or programming concept, or check spelling."
      } catch (error) {
        clearTimeout(timeout)
        if (error instanceof Error && error.name === "AbortError") {
          throw new Error("Code search request timed out after 30 seconds")
        }
        throw error
      }
    },
  })
}
