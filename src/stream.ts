export class DualStream {
  private buffer = ""

  writeToken(token: string): void {
    process.stdout.write(`\x1b[36m${token}\x1b[0m`)
    this.buffer += token
  }

  // Call when the AI SDK signals message boundary (onStepFinish or stream end)
  // Returns the complete buffered message, resets buffer
  flushMessage(): string {
    const content = this.buffer
    this.buffer = ""
    return content
  }

  // Print a status line in dim gray (not buffered to transcript)
  status(msg: string): void {
    process.stderr.write(`\x1b[2m${msg}\x1b[0m\n`)
  }

  // Print a permission prompt line
  prompt(msg: string): void {
    process.stdout.write(`\x1b[33m${msg}\x1b[0m`)
  }
}
