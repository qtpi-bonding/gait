// Minimal fake MCP server for tests — speaks MCP stdio protocol (JSON-RPC over newline-delimited stdout)
// Responds to initialize and tools/list, then exits after first tools/call or on stdin close.

import readline from "readline"

const TOOLS = [
  {
    name: "search",
    description: "Search the knowledge base",
    inputSchema: {
      type: "object",
      properties: { query: { type: "string" } },
      required: ["query"],
    },
  },
  {
    name: "get_concept",
    description: "Fetch a concept by URI",
    inputSchema: {
      type: "object",
      properties: { uri: { type: "string" } },
      required: ["uri"],
    },
  },
]

function send(msg: object) {
  process.stdout.write(JSON.stringify(msg) + "\n")
}

const rl = readline.createInterface({ input: process.stdin, terminal: false })

rl.on("line", (line) => {
  let msg: any
  try { msg = JSON.parse(line) } catch { return }

  if (msg.method === "initialize") {
    send({
      jsonrpc: "2.0", id: msg.id,
      result: {
        protocolVersion: "2024-11-05",
        capabilities: { tools: {} },
        serverInfo: { name: "fake-mcp", version: "0.0.1" },
      },
    })
  } else if (msg.method === "notifications/initialized") {
    // no response needed
  } else if (msg.method === "tools/list") {
    send({ jsonrpc: "2.0", id: msg.id, result: { tools: TOOLS } })
  } else if (msg.method === "tools/call") {
    send({
      jsonrpc: "2.0", id: msg.id,
      result: { content: [{ type: "text", text: `called ${msg.params?.name} with ${JSON.stringify(msg.params?.arguments)}` }] },
    })
    rl.close()
    process.exit(0)
  } else if (msg.id !== undefined) {
    send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: "method not found" } })
  }
})

rl.on("close", () => process.exit(0))
