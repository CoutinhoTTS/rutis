// Measures the current synchronous value-method path, not service-cache reads.
import { cpus } from 'node:os'
import { resolve } from 'node:path'
import { Process } from '../src/client.mjs'

if (!process.argv[2]) throw new Error('usage: node bench/sync-call.mjs <rutis-counter executable>')
const peer = await Process.launch(resolve(process.argv[2]), { initial: 1 })
try {
  for (let i = 0; i < 200; i++) peer.call('counter', 'current', [])
  const samples = []
  const start = performance.now()
  const resumed = new Promise(done => setImmediate(() => done(performance.now() - start)))
  for (let i = 0; i < 3000; i++) {
    const before = performance.now()
    peer.call('counter', 'current', [])
    samples.push((performance.now() - before) * 1000)
  }
  const blockedMs = await resumed
  samples.sort((a, b) => a - b)
  console.log(JSON.stringify({
    node: process.version, cpu: cpus()[0].model, executable: resolve(process.argv[2]),
    warmup: 200, samples: samples.length,
    p50_us: samples[1500], p95_us: samples[2850], p99_us: samples[2970],
    mean_us: samples.reduce((a, b) => a + b, 0) / samples.length, blocked_ms: blockedMs,
  }, null, 2))
} finally { await peer.dispose() }
