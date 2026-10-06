import { test } from 'node:test'
import assert from 'node:assert/strict'
import { setTimeout as delay } from 'node:timers/promises'
import { mkdtemp, writeFile, rm } from 'node:fs/promises'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { Context } from '@deepseek-ai/cordis'
import ts from 'typescript'
import { Process } from '../../src/client.mjs'

const { plugin } = await import(pathToFileURL(process.env.RUTIS_BINDINGS).href)
const executable = process.env.RUTIS_EXECUTABLE
const original = plugin(executable)

test('Rust disposal releases an in-flight call before waiting for it', async () => {
  const peer = await Process.launch(executable, { initial: 7 })
  let timeout
  try {
    const pending = peer.callAsync('counter', 'wait_for_dispose', [])
    void pending.catch(() => {})
    const completed = (async () => {
      while (!peer.call('counter', 'waiting', [])) await delay(1)
      await peer.dispose()
      assert.equal(await pending, 7)
    })()
    await Promise.race([completed, new Promise((_, reject) => {
      timeout = setTimeout(() => reject(new Error('disposer did not release the pending call')), 2000)
    })])
  } catch (error) {
    // This test owns the child; also bound the lifetime of a regressed server.
    try { process.kill(peer.pid, 'SIGKILL') } catch {}
    await peer.dispose().catch(() => {})
    throw error
  } finally { clearTimeout(timeout) }
})

test('generated declarations augment the real Cordis Context with native method shapes', async () => {
  const temporary = await mkdtemp(fileURLToPath(new URL('./types-', import.meta.url)))
  try {
    const source = `${temporary}/consumer.mts`
    await writeFile(source, `
      import { Context } from '@deepseek-ai/cordis'
      import { plugin } from ${JSON.stringify(process.env.RUTIS_BINDINGS)}
      const ctx = new Context()
      ctx.plugin(plugin('unused'), { initial: 1 })
      const immediate: number = ctx.counter.add(1)
      const later: Promise<number> = ctx.counter.delayedAdd(2)
      // @ts-expect-error async methods cannot be used as synchronous values
      const invalid: number = ctx.counter.delayedAdd(2)
      // @ts-expect-error original configuration is numeric
      ctx.plugin(plugin('unused'), { initial: 'bad' })
    `)
    const program = ts.createProgram([source], {
      target: ts.ScriptTarget.ESNext, module: ts.ModuleKind.NodeNext,
      moduleResolution: ts.ModuleResolutionKind.NodeNext, strict: true, noEmit: true, skipLibCheck: false,
    })
    assert.deepEqual(ts.getPreEmitDiagnostics(program).map(d => ts.flattenDiagnosticMessageText(d.messageText, '\n')), [])
  } finally { await rm(temporary, { recursive: true, force: true }) }
})

test('Cordis uses original Rust state, sync values, async results and errors', async () => {
  const ctx = new Context()
  const mounted = ctx.plugin(original, { initial: 10 })
  try {
    await mounted.await()
    const counter = ctx.counter
    assert.equal(counter.add(2), 12)
    const first = counter.delayedAdd(1)
    assert.ok(first instanceof Promise)
    const second = counter.delayedAdd(2)
    assert.equal(typeof counter.current(), 'number')
    let timerRan = false
    const timer = delay(1).then(() => { timerRan = true })
    await Promise.all([first, second, timer])
    assert.equal(timerRan, true)
    assert.equal(counter.current(), 15)
    assert.throws(() => counter.fail(), { name: 'RustError', message: 'counter refused operation' })
    assert.throws(() => counter.add(NaN), /non-finite/)
    assert.equal(counter.current(), 15)
    assert.equal(counter.reset(), undefined)
    assert.equal(counter.current(), 0)
    await mounted.dispose()
    assert.equal(ctx.get('counter'), undefined)
    assert.throws(() => counter.current(), /disposed|disconnected|exited/)
  } finally { await ctx.fiber.dispose() }
})

test('native dependency cleanup can still use the Rust service after an await', async () => {
  const ctx = new Context()
  const observed = []
  const consumer = ctx.plugin({
    name: 'ordinary-cordis-consumer', inject: ['counter'],
    apply(scope) {
      const counter = scope.counter
      observed.push(counter.add(1))
      scope.effect(() => async () => {
        await delay(15)
        observed.push(counter.current())
      })
    },
  })
  try {
    await consumer.await()
    assert.equal(consumer.store, undefined)
    const mounted = ctx.plugin(original, { initial: 3 })
    await mounted.await()
    await consumer.await()
    assert.deepEqual(observed, [4])
    await mounted.dispose()
    assert.deepEqual(observed, [4, 4])
    assert.equal(consumer.store, undefined)
  } finally { await ctx.fiber.dispose() }
})

test('native startup failure publishes no proxy', async () => {
  const ctx = new Context()
  try {
    const mounted = ctx.plugin(original, { initial: -1 })
    await assert.rejects(mounted.await(), /initial value must be non-negative/)
    assert.equal(ctx.get('counter'), undefined)
  } finally { await ctx.fiber.dispose() }
})

test('isolated Cordis mounts have separate native Rust state and lifetimes', async () => {
  const root = new Context()
  try {
    const left = root.isolate('counter')
    const right = root.isolate('counter')
    const a = left.plugin(original, { initial: 1 })
    const b = right.plugin(original, { initial: 20 })
    await Promise.all([a.await(), b.await()])
    assert.equal(left.counter.add(2), 3)
    assert.equal(right.counter.current(), 20)
    await a.dispose()
    assert.equal(left.get('counter'), undefined)
    assert.equal(right.counter.add(1), 21)
  } finally { await root.fiber.dispose() }
})

test('actual child exit fails in-flight and subsequent calls without stranding the worker', async () => {
  const peer = await Process.launch(executable, { initial: 1 })
  // Keep this call pending even if the CI scheduler pauses the Node thread.
  process.kill(peer.pid, 'SIGSTOP')
  const pending = peer.callAsync('counter', 'delayed_add', [1])
  // Kill the owned test child while a real call is pending.
  process.kill(peer.pid, 'SIGKILL')
  assert.throws(() => peer.call('counter', 'current', []), /disconnected|exited|EPIPE|ECONNRESET/)
  await assert.rejects(pending, /disconnected|exited|EPIPE|ECONNRESET/)
  await assert.rejects(peer.dispose(), /disconnected|exited|EPIPE|ECONNRESET/)
})
