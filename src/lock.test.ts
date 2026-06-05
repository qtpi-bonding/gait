import { describe, it, expect, beforeEach, afterEach } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { acquireLock, releaseLock } from "./lock"

let tmpDir: string

beforeEach(() => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "gait-lock-"))
})

afterEach(() => {
  fs.rmSync(tmpDir, { recursive: true, force: true })
})

describe("acquireLock", () => {
  it("creates a lock file with the current PID", () => {
    acquireLock(tmpDir)
    const lp = path.join(tmpDir, "lock")
    expect(fs.existsSync(lp)).toBe(true)
    expect(parseInt(fs.readFileSync(lp, "utf-8").trim(), 10)).toBe(process.pid)
  })

  it("throws when a live process holds the lock", () => {
    const lp = path.join(tmpDir, "lock")
    fs.writeFileSync(lp, String(process.pid), "utf-8") // write our own live PID
    expect(() => acquireLock(tmpDir)).toThrow(/already running/)
  })

  it("clears a stale lock (nonexistent PID) and acquires", () => {
    const lp = path.join(tmpDir, "lock")
    fs.writeFileSync(lp, "9999999", "utf-8") // PID that almost certainly doesn't exist
    // Should not throw — stale lock is removed and new one written
    expect(() => acquireLock(tmpDir)).not.toThrow()
    expect(parseInt(fs.readFileSync(lp, "utf-8").trim(), 10)).toBe(process.pid)
  })

  it("acquires when no lock file exists", () => {
    expect(() => acquireLock(tmpDir)).not.toThrow()
    expect(fs.existsSync(path.join(tmpDir, "lock"))).toBe(true)
  })
})

describe("releaseLock", () => {
  it("removes the lock file", () => {
    acquireLock(tmpDir)
    releaseLock(tmpDir)
    expect(fs.existsSync(path.join(tmpDir, "lock"))).toBe(false)
  })

  it("is safe to call when no lock exists", () => {
    expect(() => releaseLock(tmpDir)).not.toThrow()
  })

  it("is safe to call twice", () => {
    acquireLock(tmpDir)
    releaseLock(tmpDir)
    expect(() => releaseLock(tmpDir)).not.toThrow()
  })
})
