#!/usr/bin/env node
// Runs the rutis-host binary of this platform's package, with the Node
// runtime installed next to it (@arcships/rutis-runtime) as the fallback for
// projects that have none of their own.
import { spawn } from 'node:child_process'
import { createRequire } from 'node:module'
import { dirname } from 'node:path'

const require = createRequire(import.meta.url)
const platform = `${process.platform}-${process.arch}`
let binary
try {
  binary = require.resolve(`@arcships/rutis-host-${platform}/bin/rutis-host`)
} catch {
  process.stderr.write(`rutis-host: no binary for ${platform} (Linux and macOS on x64 and arm64 are supported; on Windows, use WSL)\n`)
  process.exit(1)
}
const env = { ...process.env }
if (!env.RUTIS_NODE_RUNTIME) {
  try { env.RUTIS_NODE_RUNTIME = dirname(require.resolve('@arcships/rutis-runtime/package.json')) } catch {}
}
const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit', env })
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) process.on(signal, () => child.kill(signal))
child.on('exit', (code, signal) => {
  if (signal) process.kill(process.pid, signal)
  else process.exit(code ?? 1)
})
