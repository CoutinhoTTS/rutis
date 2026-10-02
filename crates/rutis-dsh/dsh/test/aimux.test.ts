// The `llm-aimux` profile row against a real dsh-llm, with a stand-in for
// the rutis host's `aimux` service. Settings changes are driven the way the
// loader does: the row's volatile config is updated in place and
// `loader/volatile-update` announces it.
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { Context } from '@deepseek-ai/cordis'
import { updateVolatile } from '@deepseek-ai/cosmokit'
import Invariants from '@deepseek-ai/dsh-invariants'
import TypertRegistry from '@deepseek-ai/dsh-typert-registry'
import LlmRuntime from '@deepseek-ai/dsh-llm'
import * as row from '../aimux/src/index.ts'
import type { Route } from '../aimux/src/adapter.ts'

type Fiber = ReturnType<Context['plugin']> & { config: { providers: Parameters<typeof updateVolatile>[0] } }

async function host(providers: Record<string, Route>) {
  const ctx = new Context()
  ctx.plugin(Invariants)
  ctx.plugin(TypertRegistry)
  ctx.plugin(LlmRuntime)
  ctx.provide('aimux', {
    async open() { return 'call' },
    async next() { return [] },
    close() {},
    async listModels() { return [] },
  })
  const fiber = ctx.plugin(row, { providers }) as Fiber
  await fiber.await()
  return { ctx, fiber }
}

/** Change the row's routes as a settings save does. */
function save(ctx: Context, fiber: Fiber, providers: Record<string, Route>) {
  updateVolatile(fiber.config.providers, row.Config({ providers }).providers as never)
  ctx.emit('loader/volatile-update' as never)
}

const routes = (ctx: Context) => (ctx.get('llm') as { listProviders(): { id: string, name: string }[] })
  .listProviders().filter(provider => provider.id.startsWith('aimux')).map(provider => `${provider.id}:${provider.name}`)

async function settle(ctx: Context, expected: string[]) {
  for (let i = 0; i < 200 && JSON.stringify(routes(ctx)) !== JSON.stringify(expected); i++) {
    await new Promise(resolve => setTimeout(resolve, 10))
  }
  await new Promise(resolve => setTimeout(resolve, 50))
  assert.deepEqual(routes(ctx), expected)
}

/** Collects what the row reports on stderr while `run` executes. */
async function reported(run: () => Promise<void>) {
  const lines: string[] = []
  const write = console.error
  console.error = (...args: unknown[]) => { lines.push(args.join(' ')) }
  try { await run() } finally { console.error = write }
  return lines
}

const ACTIVE = 2 // FiberState.ACTIVE

test('the row starts without routes and registers the first one added later', async () => {
  const { ctx, fiber } = await host({})
  assert.equal(fiber.state, ACTIVE)
  assert.deepEqual(routes(ctx), [])
  const failures = await reported(async () => {
    save(ctx, fiber, { aimux: { displayName: 'aimux (rutis)' } })
    await settle(ctx, ['aimux:aimux (rutis)'])
  })
  assert.deepEqual(failures, [])
})

test('saves announced back to back leave no stale adapter behind', async () => {
  const { ctx, fiber } = await host({ aimux: { displayName: 'first' } })
  const failures = await reported(async () => {
    // Two saves before the first is applied, then one more later.
    save(ctx, fiber, { aimux: { displayName: 'second' } })
    save(ctx, fiber, { aimux: { displayName: 'third' } })
    await settle(ctx, ['aimux:third'])
    save(ctx, fiber, { aimux: { displayName: 'fourth' } })
    await settle(ctx, ['aimux:fourth'])
  })
  assert.deepEqual(failures, [])
})

test('removing every route and adding one again', async () => {
  const { ctx, fiber } = await host({ aimux: { displayName: 'first' } })
  const failures = await reported(async () => {
    save(ctx, fiber, {})
    await settle(ctx, [])
    save(ctx, fiber, { 'aimux-again': { displayName: 'again' } })
    await settle(ctx, ['aimux-again:again'])
  })
  assert.deepEqual(failures, [])
})

test('a route another adapter already serves is reported, not silently kept', async () => {
  const { ctx, fiber } = await host({ aimux: { displayName: 'first' } })
  const llm = ctx.get('llm') as { registerAdapter(routes: string[], adapter: object): unknown }
  const { LlmAdapter } = await import('@deepseek-ai/dsh-llm')
  llm.registerAdapter(['taken'], new (class extends LlmAdapter { async *stream() {} })())
  const failures = await reported(async () => {
    save(ctx, fiber, { taken: { displayName: 'mine' } })
    for (let i = 0; i < 200 && !routes(ctx).length; i++) await new Promise(resolve => setTimeout(resolve, 10))
    await new Promise(resolve => setTimeout(resolve, 100))
  })
  assert.equal(failures.length, 1, failures.join('\n'))
  assert.match(failures[0]!, /\[llm-aimux\] route update failed: .*taken/)
})
