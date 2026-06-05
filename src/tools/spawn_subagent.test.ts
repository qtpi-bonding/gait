import { describe, it, expect, beforeEach } from "bun:test"
import { createSpawnSubagentTool, registerSubagentRunner } from "./spawn_subagent"

// Reset module state between tests by re-registering or not registering
beforeEach(() => {
  // Unregister any previously registered runner by registering a no-op
  // (module state persists across tests since it's the same import)
})

describe("createSpawnSubagentTool", () => {
  it("throws if runner is not registered", async () => {
    // Register null by exploiting the module — use a cast to reset
    registerSubagentRunner(null as any)
    const tool = createSpawnSubagentTool("parent-session", "/workspace")
    await expect(
      tool.execute!({ task: "do something", context_files: [] }, {} as any)
    ).rejects.toThrow("subagent runner not registered")
  })

  it("calls the registered runner with correct args", async () => {
    let capturedOpts: any = null
    registerSubagentRunner(async (opts) => {
      capturedOpts = opts
      return { status: "done", result: "all good" }
    })

    const tool = createSpawnSubagentTool("parent-s1", "/my/workspace")
    await tool.execute!({ task: "refactor auth module", context_files: ["src/auth.ts"] }, {} as any)

    expect(capturedOpts).not.toBeNull()
    expect(capturedOpts.task).toBe("refactor auth module")
    expect(capturedOpts.contextFiles).toEqual(["src/auth.ts"])
    expect(capturedOpts.workspacePath).toBe("/my/workspace")
    expect(capturedOpts.sessionId).toMatch(/^parent-s1-sub-/)
  })

  it("returns JSON with session_id, status, and result", async () => {
    registerSubagentRunner(async () => ({ status: "done", result: "task complete" }))
    const tool = createSpawnSubagentTool("parent-s1", "/workspace")
    const raw = await tool.execute!({ task: "do it", context_files: [] }, {} as any)
    const parsed = JSON.parse(raw as string)
    expect(parsed.status).toBe("done")
    expect(parsed.result).toBe("task complete")
    expect(parsed.session_id).toMatch(/^parent-s1-sub-/)
  })

  it("propagates error status from runner", async () => {
    registerSubagentRunner(async () => ({ status: "error", result: "something broke" }))
    const tool = createSpawnSubagentTool("parent-s1", "/workspace")
    const raw = await tool.execute!({ task: "do it", context_files: [] }, {} as any)
    const parsed = JSON.parse(raw as string)
    expect(parsed.status).toBe("error")
    expect(parsed.result).toBe("something broke")
  })

  it("generates a unique session_id per call", async () => {
    registerSubagentRunner(async () => ({ status: "done", result: "" }))
    const tool = createSpawnSubagentTool("parent-s1", "/workspace")
    const r1 = JSON.parse(await tool.execute!({ task: "t1", context_files: [] }, {} as any) as string)
    const r2 = JSON.parse(await tool.execute!({ task: "t2", context_files: [] }, {} as any) as string)
    expect(r1.session_id).not.toBe(r2.session_id)
  })
})
