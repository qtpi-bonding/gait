import fs from "fs"
import path from "path"

function lockPath(sessionDir: string): string {
  return path.join(sessionDir, "lock")
}

function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

export function acquireLock(sessionDir: string): void {
  const lp = lockPath(sessionDir)
  if (fs.existsSync(lp)) {
    const pid = parseInt(fs.readFileSync(lp, "utf-8").trim(), 10)
    if (!isNaN(pid) && isProcessAlive(pid)) {
      throw new Error(`Session is already running (PID ${pid}). If this is wrong, delete ${lp}`)
    }
    // Stale lock — remove it
    fs.unlinkSync(lp)
  }
  fs.writeFileSync(lp, String(process.pid), "utf-8")
}

export function releaseLock(sessionDir: string): void {
  const lp = lockPath(sessionDir)
  try {
    fs.unlinkSync(lp)
  } catch {
    // already gone — fine
  }
}
