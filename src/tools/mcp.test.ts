import { describe, it, expect } from "bun:test"
import path from "path"
import { createMcpTools } from "./mcp"

const fakeServerCmd = "bun"
const fakeServerArgs = [path.join(import.meta.dir, "mcp_fake_server.ts")]

describe("createMcpTools", () => {
  it("returns tools namespaced by server name", async () => {
    const { tools, close } = await createMcpTools([
      { name: "fake", cmd: fakeServerCmd, args: fakeServerArgs, env: {} },
    ])
    expect("fake__search" in tools).toBe(true)
    expect("fake__get_concept" in tools).toBe(true)
    await close()
  })

  it("does not include un-namespaced tool names", async () => {
    const { tools, close } = await createMcpTools([
      { name: "fake", cmd: fakeServerCmd, args: fakeServerArgs, env: {} },
    ])
    expect("search" in tools).toBe(false)
    expect("get_concept" in tools).toBe(false)
    await close()
  })

  it("returns empty tools for empty server list", async () => {
    const { tools, close } = await createMcpTools([])
    expect(Object.keys(tools)).toHaveLength(0)
    await close()
  })

  it("tool is callable and returns a result containing the tool name", async () => {
    const { tools, close } = await createMcpTools([
      { name: "fake", cmd: fakeServerCmd, args: fakeServerArgs, env: {} },
    ])
    const result = await (tools["fake__search"] as any).execute({ query: "HNSW" }, {} as any)
    const text = typeof result === "string" ? result : JSON.stringify(result)
    expect(text).toContain("search")
    await close()
  })
})
