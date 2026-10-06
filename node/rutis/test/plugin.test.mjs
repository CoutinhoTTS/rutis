import { test } from 'node:test'
import assert from 'node:assert/strict'
import { definePlugin, isPlugin, PLUGIN_API } from '../src/index.mjs'
import { load, PluginTestError } from '../src/testing.mjs'

class Weather {
  constructor(llm, city) { this.llm = llm; this.city = city }
  async today() { return `${await this.llm.ask(this.city)} in ${this.city}` }
  forecast(days) { return days.map(day => `${day}: fine`) }
}

const weather = definePlugin({
  inject: ['llm'],
  provides: { weather: { today: 'async', forecast: 'sync' } },
  apply(ctx, config) {
    ctx.provide('weather', new Weather(ctx.use('llm'), config.city))
    // Latest first, as the runtime runs them: the returned cleanup, then this.
    let cleaned = false
    ctx.effect(() => { if (!cleaned) throw new Error('the returned cleanup runs first') })
    return () => { cleaned = true }
  },
})

test('definePlugin checks the declaration and stamps the plugin API', () => {
  assert.ok(isPlugin(weather))
  assert.equal(weather.api, PLUGIN_API)
  assert.throws(() => definePlugin({}), /apply/)
  assert.throws(() => definePlugin({ apply() {}, provides: { a: { b: 'maybe' } } }), /sync' or 'async/)
})

test('a plugin runs with the services it injects and provides what it declares', async () => {
  const t = await load(weather, { config: { city: 'Oslo' }, services: { llm: { ask: async () => 'sunny' } } })
  assert.equal(await t.service('weather').today(), 'sunny in Oslo')
  assert.deepEqual(t.service('weather').forecast(['mon']), ['mon: fine'])
  assert.deepEqual(t.provided(), ['weather'])
  await t.unload()
  assert.deepEqual(t.provided(), [])
})

test('undeclared services, missing services and missing methods are errors', async () => {
  await assert.rejects(load(weather, { config: {} }), /injects llm/)
  const sneaky = definePlugin({ apply(ctx) { ctx.use('llm') } })
  await assert.rejects(load(sneaky, { services: { llm: {} } }), /without declaring it in inject/)
  const incomplete = definePlugin({ provides: { a: { run: 'sync' } }, apply(ctx) { ctx.provide('a', {}) } })
  await assert.rejects(load(incomplete), /no such method/)
  const t = await load(weather, { config: { city: 'x' }, services: { llm: { ask: async () => '' } } })
  assert.throws(() => t.service('other'), PluginTestError)
})

test('strict mode crosses values as between processes', async () => {
  const kept = []
  const plugin = definePlugin({
    provides: { store: { put: 'sync', take: 'sync', wrong: 'sync' } },
    apply(ctx) {
      ctx.provide('store', {
        put(item) { kept.push(item) },
        take() { return kept[0] },
        wrong() { return Promise.resolve(1) },
      })
    },
  })
  const t = await load(plugin)
  const item = { n: 1 }
  t.service('store').put(item)
  item.n = 2
  assert.equal(t.service('store').take().n, 1, 'data is copied, not shared')
  assert.throws(() => t.service('store').put(1n), /bigint cannot cross/)
  assert.throws(() => t.service('store').wrong(), /declared sync but returned a promise/)
})

test('a plugin that needs a newer plugin API is refused', async () => {
  const future = { ...weather, api: PLUGIN_API + 1 }
  await assert.rejects(load(future, { services: { llm: {} } }), /needs plugin API/)
})
