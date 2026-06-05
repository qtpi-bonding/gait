import { describe, it, expect } from "bun:test"
import { parseConfig } from "./config"

describe("config", () => {
  it("parses minimal config", () => {
    const cfg = parseConfig(`{ "model": "anthropic/claude-sonnet-4-6" }`)
    expect(cfg.model).toBe("anthropic/claude-sonnet-4-6")
    expect(cfg.allow).toEqual([])
    expect(cfg.tools).toEqual([])
  })

  it("parses allow list", () => {
    const cfg = parseConfig(`{ "model": "anthropic/claude-sonnet-4-6", "allow": ["read", "glob"] }`)
    expect(cfg.allow).toEqual(["read", "glob"])
  })

  it("parses tier 1 config-defined tools", () => {
    const cfg = parseConfig(`{
      "model": "anthropic/claude-sonnet-4-6",
      "tools": [{ "name": "run_tests", "description": "Run tests", "cmd": "bun test 2>&1" }]
    }`)
    expect(cfg.tools[0]!.name).toBe("run_tests")
    expect(cfg.tools[0]!.cwd).toBe("codebase")
  })

  it("strips JSON comments", () => {
    const cfg = parseConfig(`{
      // this is a comment
      "model": "anthropic/claude-sonnet-4-6" // inline comment
    }`)
    expect(cfg.model).toBe("anthropic/claude-sonnet-4-6")
  })

  it("rejects missing model", () => {
    expect(() => parseConfig(`{ "allow": [] }`)).toThrow()
  })

  it("rejects tool without cmd", () => {
    expect(() => parseConfig(`{
      "model": "anthropic/claude-sonnet-4-6",
      "tools": [{ "name": "run_tests", "description": "Run tests" }]
    }`)).toThrow()
  })
})
