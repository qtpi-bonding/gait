const [, , cmd, ...args] = process.argv

switch (cmd) {
  case "init":
    console.log("init — not yet implemented")
    break
  case "run":
    console.log("run — not yet implemented")
    break
  case "auth":
    console.log("auth — not yet implemented")
    break
  case "models":
    console.log("models — not yet implemented")
    break
  default:
    console.log(`ainix <init|run|auth|models>`)
    process.exit(cmd ? 1 : 0)
}
