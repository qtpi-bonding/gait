import { experimental_createMCPClient } from "ai"
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js"
import type { CoreTool } from "ai"
import type { McpServerConfig } from "../config"

export async function createMcpTools(
  servers: McpServerConfig[],
): Promise<{ tools: Record<string, CoreTool>; close: () => Promise<void> }> {
  if (servers.length === 0) {
    return { tools: {}, close: async () => {} }
  }

  const clients: Awaited<ReturnType<typeof experimental_createMCPClient>>[] = []
  const tools: Record<string, CoreTool> = {}

  for (const server of servers) {
    const transport = new StdioClientTransport({
      command: server.cmd,
      args: server.args,
      env: { ...process.env, ...server.env } as Record<string, string>,
    })

    const client = await experimental_createMCPClient({ transport })
    clients.push(client)

    const serverTools = await client.tools()
    for (const [name, tool] of Object.entries(serverTools)) {
      tools[`${server.name}__${name}`] = tool as CoreTool
    }
  }

  return {
    tools,
    close: async () => {
      await Promise.all(clients.map((c) => c.close()))
    },
  }
}
