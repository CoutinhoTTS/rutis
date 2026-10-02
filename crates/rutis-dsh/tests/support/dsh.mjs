// Reference results from dsh's own JavaScript, for parity tests.
//   node dsh.mjs yaml <file>...       → [{ file, value }] parsed with the entry-list dialect
//   node dsh.mjs compose <json>       → { rows, layers } for a profile context
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const project = join(here, '../../dsh')
const require = createRequire(join(project, 'package.json'))
const load = name => import(pathToFileURL(require.resolve(name)).href)

const [command, ...args] = process.argv.slice(2)
const yaml = await load('js-yaml').catch(() => import(pathToFileURL(join(project, 'node_modules/js-yaml/index.js')).href))
const { entryListSchema } = await load('@deepseek-ai/cordis-plugin-include')

if (command === 'yaml') {
  const out = args.map(file => ({ file, value: yaml.load(readFileSync(file, 'utf8'), { schema: entryListSchema }) ?? null }))
  process.stdout.write(JSON.stringify(out))
} else if (command === 'compose') {
  const boot = await load('@deepseek-ai/dsh-app-boot')
  const context = JSON.parse(args[0])
  const profile = boot.loadProfileDirectory('dsh', context.dir, context.installAnchor)
  const patches = boot.readProfilePatches('dsh', context, profile)
  const warnings = []
  const rows = boot.composeEntries([patches], m => warnings.push(m))
  process.stdout.write(JSON.stringify({ rows, skipped: profile.skippedBundles.map(b => b.packageName), warnings: warnings.length }))
} else {
  throw new Error(`unknown command ${command}`)
}
