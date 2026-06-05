import { Auth } from "../../auth/index"
import readline from "readline"

async function prompt(q: string): Promise<string> {
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout })
  return new Promise((res) => rl.question(q, (a) => { rl.close(); res(a.trim()) }))
}

export async function authCommand(argv: string[]): Promise<void> {
  const [sub, provider] = argv

  if (sub === "list" || sub === "ls" || !sub) {
    const credentials = await Auth.all()
    if (Object.keys(credentials).length === 0) {
      console.log("No credentials configured. Run: ainix auth add <provider>")
      return
    }
    for (const [key, cred] of Object.entries(credentials)) {
      const masked = cred.type === "api" ? `${cred.key.slice(0, 8)}...` : cred.type
      console.log(`  ${key}: ${masked}`)
    }
    return
  }

  if (sub === "add") {
    if (!provider) { console.error("Usage: ainix auth add <provider>"); process.exit(1) }
    const key = await prompt(`${provider} API key: `)
    await Auth.set(provider, { type: "api", key })
    console.log(`Saved ${provider} credentials`)
    return
  }

  if (sub === "remove" || sub === "rm") {
    if (!provider) { console.error("Usage: ainix auth remove <provider>"); process.exit(1) }
    await Auth.remove(provider)
    console.log(`Removed ${provider} credentials`)
    return
  }

  console.error(`Unknown auth command: ${sub}`)
  process.exit(1)
}
