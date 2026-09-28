import { test } from 'node:test'
import assert from 'node:assert/strict'
import { Context } from '@deepseek-ai/cordis'

// Public-API coverage evidence for design §4.1 / §8, pinned to Cordis 4.0.1.
// These checks do not replace methods or mutate framework internals.
test('internal/get covers injected properties, while get, root and accessors use other paths', async t => {
  const root = new Context()
  t.after(() => root.fiber.dispose())
  root.provide('counter', 1)
  const reads = []
  root.on('internal/get', (ctx, name, error, next) => {
    reads.push(name)
    return next()
  })
  let computedReads = 0
  root.accessor('computed', { get() { return ++computedReads } })
  let scope
  const mounted = root.plugin({ inject: ['counter'], apply(ctx) { scope = ctx } })
  await mounted.await()
  reads.length = 0

  assert.equal(scope.counter, 1)
  assert.deepEqual(reads, ['counter'])
  reads.length = 0
  assert.equal(scope.get('counter'), 1)
  assert.equal(scope.get('counter', false), 1)
  assert.equal(root.counter, 1)
  assert.equal(scope.computed, 1)
  assert.equal(scope.computed, 2)
  assert.deepEqual(reads, [])
})

test('internal/set wraps property writes and downstream hooks, but direct set bypasses it', async t => {
  const root = new Context()
  t.after(() => root.fiber.dispose())
  root.provide('counter', 1)
  const trace = []
  root.on('internal/set', (ctx, name, value, error, next) => {
    trace.push(['outer-before', ctx.get(name)])
    const result = next()
    trace.push(['outer-after', ctx.get(name)])
    return result
  })
  root.on('internal/set', (ctx, name, value, error, next) => {
    trace.push(['user-before', ctx.get(name)])
    const result = next()
    trace.push(['user-after', ctx.get(name)])
    return result
  })
  root.counter = 2
  assert.deepEqual(trace, [
    ['outer-before', 1], ['user-before', 1], ['user-after', 2], ['outer-after', 2],
  ])
  trace.length = 0
  root.set('counter', 3)
  assert.equal(root.counter, 3)
  assert.deepEqual(trace, [])
})

test('internal/listener replaces registration and can retain once and fiber-owned cleanup', async t => {
  const root = new Context()
  t.after(() => root.fiber.dispose())
  const registrations = new Set()
  root.on('internal/listener', function (name, callback, options) {
    if (name !== 'audit-once' && name !== 'audit-owned') return
    return this.effect(() => {
      const entry = { name, callback, options }
      registrations.add(entry)
      return () => registrations.delete(entry)
    })
  })
  let calls = 0
  const mounted = root.plugin(ctx => {
    ctx.once('audit-once', () => {
      calls++
      assert.equal([...registrations].some(entry => entry.name === 'audit-once'), false)
    }, { prepend: true, global: true })
    ctx.on('audit-owned', () => { calls++ })
  })
  await mounted.await()
  assert.equal(registrations.size, 2)
  root.emit('audit-once')
  root.emit('audit-owned')
  assert.equal(calls, 0, 'replaced registrations are absent from the native dispatch list')
  const once = [...registrations].find(entry => entry.name === 'audit-once')
  assert.deepEqual(once.options, { prepend: true, global: true })
  once.callback()
  assert.equal(calls, 1)
  assert.equal(registrations.size, 1)
  await mounted.dispose()
  assert.equal(registrations.size, 0)
})

test('internal/update can return forwarded completion without a local restart', async t => {
  const root = new Context()
  t.after(() => root.fiber.dispose())
  let starts = 0
  const updates = []
  const completion = Promise.resolve()
  const mounted = await root.plugin(ctx => {
    starts++
    ctx.on('internal/update', (nextConfig, noSave, next) => {
      updates.push([nextConfig, noSave, typeof next])
      return completion
    })
  }, { value: 1 })
  await mounted.await()
  const result = mounted.update({ value: 2 }, true)
  assert.equal(result, completion)
  await result
  assert.deepEqual(updates, [[{ value: 2 }, true, 'function']])
  assert.equal(starts, 1)
})

test('Pending update bypasses internal/update and applies the latest config on activation', async t => {
  const root = new Context()
  t.after(() => root.fiber.dispose())
  let updates = 0
  root.on('internal/update', (config, noSave, next) => { updates++; return next() }, { global: true })
  const applied = []
  // Await the public thenable to obtain the actual fiber, not its wrapper.
  const mounted = await root.plugin({
    inject: ['dependency'],
    apply(ctx, config) { applied.push(config) },
  }, { value: 1 })
  await mounted.await()
  assert.deepEqual(applied, [])
  mounted.update({ value: 2 })
  assert.equal(updates, 0)
  root.provide('dependency', true)
  await mounted.await()
  assert.deepEqual(applied, [{ value: 2 }])
})

test('internal/dispatch cannot replace native dispatch or its result and conflates emit with parallel', async t => {
  const root = new Context()
  t.after(() => root.fiber.dispose())
  const modes = []
  root.on('internal/dispatch', (mode, name) => {
    if (name === 'audit') modes.push(mode)
    return 'observer-result'
  })
  let localCalls = 0
  root.on('audit', () => { localCalls++; return 'local-result' })
  root.emit('audit')
  await root.parallel('audit')
  assert.equal(root.bail('audit'), 'local-result')
  assert.deepEqual(modes, ['emit', 'emit', 'bail'])
  assert.equal(localCalls, 3)
})

test('service and status notifications observe already committed registration and availability', async t => {
  const root = new Context()
  t.after(() => root.fiber.dispose())
  const services = []
  root.on('internal/service', function (name, value) {
    if (name === 'counter') services.push([value, this.get(name, false)])
  })
  const remove = root.provide('counter', 1)
  assert.deepEqual(services, [[1, 1]])
  await remove()
  assert.deepEqual(services, [[1, 1], [undefined, undefined]])

  const statuses = []
  root.on('internal/status', (fiber, oldState) => {
    statuses.push([fiber.state, oldState])
  })
  const mounted = root.plugin(() => {})
  await mounted.await()
  await mounted.dispose()
  assert.ok(statuses.length >= 2)
  for (const [current, old] of statuses) assert.notEqual(current, old)
})
