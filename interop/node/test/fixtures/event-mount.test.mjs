import { test } from 'node:test'
import assert from 'node:assert/strict'
import { setTimeout as delay, setImmediate as tick } from 'node:timers/promises'
import { pathToFileURL } from 'node:url'
import { Context } from '@deepseek-ai/cordis'

const { plugin } = await import(pathToFileURL(process.env.RUTIS_BINDINGS).href)

// Compare Cordis's local listener contract with individual listeners calling
// generated methods on a real Rust plugin. This does not implement interception
// of original rutis event registrations or Rust-origin event dispatch.
async function scenario(remote, run) {
  const ctx = new Context()
  let finish
  try {
    if (remote) {
      const mounted = await ctx.plugin(plugin(process.env.RUTIS_EXECUTABLE), { initial: 1 })
      finish = () => mounted.dispose()
    } else {
      let value = 1, waiting = false, release
      const stopped = new Promise(resolve => { release = resolve })
      ctx.provide('counter', {
        add(amount) { return value += amount },
        current() { return value },
        fail() { throw Object.assign(new Error('counter refused operation'), { name: 'RustError' }) },
        waitForDispose() { waiting = true; return stopped.then(() => value) },
        waiting() { return waiting },
      })
      finish = async () => { release() }
    }
    return await run(ctx, ctx.counter, finish)
  } finally { await ctx.fiber.dispose() }
}

async function compare(run, expected) {
  const native = await scenario(false, run)
  const remote = await scenario(true, run)
  if (expected !== undefined) assert.deepEqual(native, expected)
  assert.deepEqual(remote, native)
}

for (const mode of ['emit', 'bail']) {
  test(`cross-process listener preserves mixed A/B/C order and ${mode} result`, async () => {
    await compare(async (ctx, counter) => {
      const trace = []
      ctx.on('audit', () => { trace.push('A') })
      ctx.on('audit', () => { trace.push('B'); return counter.add(1) })
      ctx.on('audit', () => { trace.push('C') })
      const result = ctx[mode]('audit')
      return { trace, result, value: counter.current() }
    }, { trace: mode === 'bail' ? ['A', 'B'] : ['A', 'B', 'C'], result: mode === 'bail' ? 2 : undefined, value: 2 })
  })
}

test('cross-process once listener is removed before reentrant dispatch', async () => {
  await compare(async (ctx, counter) => {
    const trace = []
    ctx.on('audit', () => { trace.push('A') })
    ctx.once('audit', () => {
      trace.push('B')
      counter.add(1)
      ctx.emit('audit')
    })
    ctx.on('audit', () => { trace.push('C') })
    ctx.emit('audit')
    return { trace, value: counter.current() }
  }, { trace: ['A', 'B', 'A', 'C', 'C'], value: 2 })
})

test('cross-process listener retains Cordis filter, prepend and explicit removal', async () => {
  await compare(async (ctx, counter) => {
    const trace = []
    const local = ctx.extend({ group: 'local' })
    const foreign = ctx.extend({ group: 'foreign' })
    local.on('audit', () => { trace.push('A') })
    const remove = foreign.on('audit', () => { trace.push('B'); counter.add(1) }, { prepend: true })
    local.on('audit', () => { trace.push('C') }, { global: true })
    const filter = { [Context.filter](listener) { return listener.group === 'foreign' } }
    ctx.emit(filter, 'audit')
    remove()
    ctx.emit(filter, 'audit')
    return { trace, value: counter.current() }
  }, { trace: ['B', 'C', 'C'], value: 2 })
})

for (const mode of ['emit', 'parallel', 'bail']) {
  test(`cross-process async listener keeps native ${mode} completion behavior`, async () => {
    await compare(async (ctx, counter, finish) => {
      const trace = []
      let promise
      ctx.on('audit', () => { trace.push('A') })
      ctx.on('audit', () => { trace.push('B'); return promise = counter.waitForDispose() })
      ctx.on('audit', () => { trace.push('C') })
      const result = ctx[mode]('audit')
      let completed = false, dispatchCompleted = false
      void promise.then(() => { completed = true })
      if (mode !== 'emit') void result.then(() => { dispatchCompleted = true })
      const deadline = Date.now() + 3000
      while (!counter.waiting()) {
        assert.ok(Date.now() < deadline, 'Rust listener did not start')
        await delay(1)
      }
      await tick()
      assert.equal(completed, false)
      assert.equal(dispatchCompleted, false)
      if (mode === 'emit') assert.equal(result, undefined)
      else assert.ok(result instanceof Promise)
      if (mode === 'bail') assert.equal(result, promise)
      await finish()
      const value = await promise
      await result
      assert.equal(dispatchCompleted, mode !== 'emit')
      return { trace, value, completed }
    }, { trace: mode === 'bail' ? ['A', 'B'] : ['A', 'B', 'C'], value: 1, completed: true })
  })
}

function errorShape(error) {
  return {
    name: error.name, message: error.message,
    cause: error.cause && errorShape(error.cause),
    errors: error.errors?.map(errorShape),
  }
}

test('native parallel aggregates a remote synchronous error and a local nested rejection once', async () => {
  await compare(async (ctx, counter) => {
    const trace = []
    ctx.on('audit', () => { trace.push('B'); return counter.fail() })
    ctx.on('audit', () => {
      trace.push('C')
      return Promise.reject(new AggregateError([new TypeError('inner')], 'nested', { cause: new Error('cause') }))
    })
    assert.throws(() => ctx.emit('audit'), { name: 'RustError', message: 'counter refused operation' })
    assert.deepEqual(trace, ['B'])
    trace.length = 0
    let shape
    await assert.rejects(ctx.parallel('audit'), error => {
      assert.ok(error instanceof AggregateError)
      assert.equal(error.errors.length, 2)
      assert.equal(error.errors[0].name, 'RustError')
      assert.ok(error.errors[1] instanceof AggregateError)
      assert.equal(error.errors[1].cause.message, 'cause')
      shape = errorShape(error)
      return true
    })
    assert.deepEqual(trace, ['B', 'C'])
    return shape
  })
})
