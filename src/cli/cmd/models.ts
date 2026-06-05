import { getModels } from "../../provider/models"

export async function modelsCommand(argv: string[]): Promise<void> {
  const [filter] = argv
  try {
    const models = await getModels()
    const filtered = filter
      ? models.filter((m) => m.provider?.toLowerCase().includes(filter.toLowerCase()) || m.id?.toLowerCase().includes(filter.toLowerCase()))
      : models
    for (const m of filtered) {
      console.log(`  ${m.provider ?? "?"}/${m.id ?? "?"}${m.name ? `  (${m.name})` : ""}`)
    }
    console.log(`\n${filtered.length} models${filter ? ` matching "${filter}"` : ""}`)
  } catch (err) {
    console.error(`Error fetching models: ${err}`)
    process.exit(1)
  }
}
