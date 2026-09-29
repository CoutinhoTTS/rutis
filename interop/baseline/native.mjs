// Runs every baseline scenario in native Cordis and prints the results as
// JSON: { [target]: { available, results: [{ ok } | { error: { name, message } }] } }.
// `node native.mjs <root>`: scenario directories are created under <root>.
import { Context } from '@deepseek-ai/cordis'
import { prepare, substitute, targets } from './scenarios.mjs'

const root = process.argv[2]
const report = {}
for (const target of targets) {
  const scenario = prepare(target, root)
  const ctx = new Context()
  const entry = { available: false, results: [] }
  try {
    const fibers = []
    for (const plugin of scenario.plugins) {
      const module = await import(plugin.package)
      fibers.push(ctx.plugin(typeof module.apply === 'function' ? module : module.default, plugin.config))
    }
    // A plugin with unresolved dependencies stays pending; do not wait for it.
    await Promise.race([Promise.all(fibers.map(fiber => fiber.await())), new Promise(resolve => setTimeout(resolve, 500))])
    const service = ctx.get(scenario.service)
    entry.available = service !== undefined
    const captured = {}
    for (const { method, args, capture } of scenario.calls) {
      try {
        const value = await Reflect.apply(service[method], service, substitute(args, captured))
        if (capture) captured[capture] = value
        entry.results.push({ ok: value === undefined ? null : value })
      } catch (error) {
        entry.results.push({ error: { name: error?.name ?? 'Error', message: String(error?.message ?? error) } })
      }
    }
  } catch (error) {
    entry.error = String(error?.message ?? error)
  }
  await ctx.fiber.dispose().catch(() => {})
  report[target.name] = entry
}
process.stdout.write(JSON.stringify(report))
process.exit(0)
