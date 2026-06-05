// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { tool } from "ai"
import { z } from "zod"

const MAX_RESPONSE_SIZE = 5 * 1024 * 1024 // 5MB
const DEFAULT_TIMEOUT = 30_000 // 30 seconds
const MAX_TIMEOUT = 120_000 // 2 minutes

function stripHTML(html: string): string {
  // Remove script, style, and other non-content tags with their content
  let text = html
    .replace(/<script[^>]*>[\s\S]*?<\/script>/gi, "")
    .replace(/<style[^>]*>[\s\S]*?<\/style>/gi, "")
    .replace(/<noscript[^>]*>[\s\S]*?<\/noscript>/gi, "")
    .replace(/<iframe[^>]*>[\s\S]*?<\/iframe>/gi, "")

  // Convert block elements to newlines
  text = text
    .replace(/<br\s*\/?>/gi, "\n")
    .replace(/<\/p>/gi, "\n\n")
    .replace(/<\/div>/gi, "\n")
    .replace(/<\/li>/gi, "\n")
    .replace(/<\/h[1-6]>/gi, "\n\n")
    .replace(/<\/tr>/gi, "\n")

  // Strip remaining tags
  text = text.replace(/<[^>]+>/g, "")

  // Decode common HTML entities
  text = text
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&nbsp;/g, " ")
    .replace(/&mdash;/g, "—")
    .replace(/&ndash;/g, "–")

  // Collapse excessive whitespace
  return text.replace(/\n{3,}/g, "\n\n").replace(/[ \t]+/g, " ").trim()
}

function htmlToMarkdown(html: string): string {
  // Simple conversion — covers the most common cases without a heavy dep
  let md = html
    .replace(/<script[^>]*>[\s\S]*?<\/script>/gi, "")
    .replace(/<style[^>]*>[\s\S]*?<\/style>/gi, "")
    .replace(/<h1[^>]*>([\s\S]*?)<\/h1>/gi, "\n# $1\n")
    .replace(/<h2[^>]*>([\s\S]*?)<\/h2>/gi, "\n## $1\n")
    .replace(/<h3[^>]*>([\s\S]*?)<\/h3>/gi, "\n### $1\n")
    .replace(/<h4[^>]*>([\s\S]*?)<\/h4>/gi, "\n#### $1\n")
    .replace(/<h5[^>]*>([\s\S]*?)<\/h5>/gi, "\n##### $1\n")
    .replace(/<h6[^>]*>([\s\S]*?)<\/h6>/gi, "\n###### $1\n")
    .replace(/<strong[^>]*>([\s\S]*?)<\/strong>/gi, "**$1**")
    .replace(/<b[^>]*>([\s\S]*?)<\/b>/gi, "**$1**")
    .replace(/<em[^>]*>([\s\S]*?)<\/em>/gi, "*$1*")
    .replace(/<i[^>]*>([\s\S]*?)<\/i>/gi, "*$1*")
    .replace(/<code[^>]*>([\s\S]*?)<\/code>/gi, "`$1`")
    .replace(/<pre[^>]*>([\s\S]*?)<\/pre>/gi, "\n```\n$1\n```\n")
    .replace(/<a[^>]*href="([^"]*)"[^>]*>([\s\S]*?)<\/a>/gi, "[$2]($1)")
    .replace(/<br\s*\/?>/gi, "\n")
    .replace(/<\/p>/gi, "\n\n")
    .replace(/<\/div>/gi, "\n")
    .replace(/<li[^>]*>/gi, "- ")
    .replace(/<\/li>/gi, "\n")
    .replace(/<\/tr>/gi, "\n")
    .replace(/<\/h[1-6]>/gi, "\n")
    .replace(/<[^>]+>/g, "")
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&nbsp;/g, " ")

  return md.replace(/\n{3,}/g, "\n\n").trim()
}

export function createWebfetchTool() {
  return tool({
    description: "Fetch content from a URL and return it as text or markdown.",
    parameters: z.object({
      url: z.string().describe("The URL to fetch content from (must start with http:// or https://)"),
      format: z
        .enum(["text", "markdown", "html"])
        .default("markdown")
        .describe("The format to return content in: 'text' (plain), 'markdown', or 'html'. Defaults to markdown."),
      timeout: z.number().optional().describe("Timeout in seconds (max 120). Defaults to 30."),
    }),
    execute: async ({ url, format = "markdown", timeout }) => {
      if (!url.startsWith("http://") && !url.startsWith("https://")) {
        throw new Error("URL must start with http:// or https://")
      }

      const timeoutMs = Math.min((timeout ?? DEFAULT_TIMEOUT / 1000) * 1000, MAX_TIMEOUT)
      const controller = new AbortController()
      const timer = setTimeout(() => controller.abort(), timeoutMs)

      let acceptHeader = "*/*"
      switch (format) {
        case "markdown":
          acceptHeader = "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
          break
        case "text":
          acceptHeader = "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1"
          break
        case "html":
          acceptHeader = "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, */*;q=0.1"
          break
      }

      const headers = {
        "User-Agent":
          "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36",
        Accept: acceptHeader,
        "Accept-Language": "en-US,en;q=0.9",
      }

      try {
        const initial = await fetch(url, { signal: controller.signal, headers })
        // Retry with honest UA if blocked by Cloudflare
        const response =
          initial.status === 403 && initial.headers.get("cf-mitigated") === "challenge"
            ? await fetch(url, { signal: controller.signal, headers: { ...headers, "User-Agent": "gait" } })
            : initial

        clearTimeout(timer)

        if (!response.ok) {
          throw new Error(`Request failed with status ${response.status}: ${response.statusText}`)
        }

        const contentLength = response.headers.get("content-length")
        if (contentLength && parseInt(contentLength) > MAX_RESPONSE_SIZE) {
          throw new Error("Response too large (exceeds 5MB limit)")
        }

        const buffer = await response.arrayBuffer()
        if (buffer.byteLength > MAX_RESPONSE_SIZE) {
          throw new Error("Response too large (exceeds 5MB limit)")
        }

        const content = new TextDecoder().decode(buffer)
        const contentType = response.headers.get("content-type") || ""
        const isHTML = contentType.includes("text/html")

        switch (format) {
          case "markdown":
            return isHTML ? htmlToMarkdown(content) : content
          case "text":
            return isHTML ? stripHTML(content) : content
          case "html":
            return content
          default:
            return content
        }
      } catch (error) {
        clearTimeout(timer)
        if (error instanceof Error && error.name === "AbortError") {
          throw new Error(`Request timed out after ${timeoutMs / 1000}s`)
        }
        throw error
      }
    },
  })
}
