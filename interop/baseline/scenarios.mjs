// Baseline scenarios: real published Cordis plugins with data-only calls.
// The same list runs natively (native.mjs) and from Rust through
// rutis-interop (crates/rutis-interop/tests/dsh_baseline.rs); results are
// compared after replacing the shared scenario root with "$DIR".
//
// A call is [method, args, capture?]. A string argument "$name" is replaced
// by the result captured under that name by an earlier call.
import { existsSync, mkdirSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

const require = createRequire(import.meta.url)

export const targets = [
  {
    name: 'invariants',
    package: '@deepseek-ai/dsh-invariants',
    service: 'invariants',
    config: () => ({}),
    calls: [],
  },
  {
    name: 'credentials',
    package: '@deepseek-ai/dsh-credentials-local',
    service: 'credentials',
    config: dir => ({ dshHome: dir, watch: false }),
    calls: [
      ['describe', ['test/api-key']],
      ['set', ['test/api-key', 's3cret']],
      ['describe', ['test/api-key']],
      ['resolve', ['test/api-key']],
      ['listRecords', []],
      ['unset', ['test/api-key']],
      ['resolve', ['test/api-key']],
    ],
  },
  {
    name: 'fs',
    package: '@deepseek-ai/dsh-fs-local',
    service: 'fs',
    config: dir => ({ cwd: dir }),
    // Idempotent: native and interop runs share one directory, so file
    // versions (inode, mtime) compare equal.
    setup: dir => existsSync(join(dir, 'a.txt')) || writeFileSync(join(dir, 'a.txt'), 'hello'),
    calls: [
      ['resolve', ['a.txt'], 'file'],
      ['resolve', ['.'], 'root'],
      ['readText', ['$file']],
      ['listDir', ['$root']],
      ['contains', ['$root', '$file']],
      ['processPath', ['$file']],
      ['resolve', ['missing.txt'], 'missing'],
      ['readText', ['$missing']],
    ],
  },
  {
    name: 'jobs',
    package: '@deepseek-ai/dsh-jobs-local',
    service: 'jobs',
    config: () => ({}),
    calls: [
      ['list', []],
      ['start', [{ name: 'x', command: 'echo', args: ['hi'] }]],
    ],
  },
  {
    name: 'commands',
    package: '@deepseek-ai/dsh-commands',
    service: 'commands',
    config: () => ({}),
    calls: [],
  },
  {
    // Requires storageDomain and sessionPersistence, which nothing provides:
    // natively the plugin stays pending and publishes no service.
    name: 'workspace',
    package: '@deepseek-ai/dsh-workspace',
    service: 'workspaceRegistry',
    config: () => ({}),
    calls: [],
  },
]

export function prepare(target, root) {
  const dir = join(root, target.name)
  mkdirSync(dir, { recursive: true })
  target.setup?.(dir)
  return {
    name: target.name,
    entry: require.resolve(target.package),
    service: target.service,
    methods: [...new Set(target.calls.map(([method]) => method))],
    config: target.config(dir),
    calls: target.calls.map(([method, args, capture]) => ({ method, args, capture })),
    dir,
  }
}

export function substitute(value, captured) {
  if (typeof value === 'string' && value.startsWith('$')) {
    const name = value.slice(1)
    return Object.hasOwn(captured, name) ? captured[name] : undefined
  }
  if (Array.isArray(value)) return value.map(item => substitute(item, captured))
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, substitute(item, captured)]))
  }
  return value
}

// `node scenarios.mjs <root>` prints the prepared scenarios for the Rust side.
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  process.stdout.write(JSON.stringify(targets.map(target => prepare(target, process.argv[2]))))
}
