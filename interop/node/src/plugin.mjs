// Leaf plugins: what a JavaScript or TypeScript plugin for rutis looks like
// when it needs no Cordis. The same shape as a Python plugin:
//
//   export default definePlugin({
//     inject: ['llm'],
//     provides: { weather: { today: 'async' } },
//     config: { type: 'object', properties: { city: { type: 'string' } } },
//     apply(ctx, config) {
//       ctx.provide('weather', new Weather(ctx.use('llm'), config.city))
//       return () => {}
//     },
//   })
//
// The runtime loads it into its Cordis Context like any plugin, so it shares
// services with Cordis plugins in the same process without IPC; the plugin
// itself only sees `ctx.use`, `ctx.provide` and `ctx.effect`.

export const LEAF = Symbol.for('rutis.leaf-plugin')

export function definePlugin(spec) {
  if (typeof spec?.apply !== 'function') throw new TypeError('a plugin needs apply(ctx, config)')
  const inject = spec.inject ?? []
  if (!Array.isArray(inject) || inject.some(name => typeof name !== 'string')) throw new TypeError('inject must be a list of service names')
  for (const [name, methods] of Object.entries(spec.provides ?? {})) {
    for (const [method, kind] of Object.entries(methods ?? {})) {
      if (kind !== 'sync' && kind !== 'async') throw new TypeError(`${name}.${method}: kind must be 'sync' or 'async'`)
    }
  }
  return Object.freeze({ ...spec, inject, provides: spec.provides ?? {}, [LEAF]: true })
}

export const isLeaf = value => value?.[LEAF] === true

// The Cordis plugin that runs a leaf plugin.
export function toCordis(spec) {
  return {
    name: spec.name ?? 'leaf',
    inject: spec.inject,
    async apply(context, config) {
      const ctx = {
        use(name) {
          const value = context.get(name)
          if (value === undefined) throw new Error(`service ${name} is not available`)
          return value
        },
        provide: (name, value) => context.provide(name, value),
        effect: cleanup => { context.effect(() => cleanup) },
      }
      const cleanup = await spec.apply(ctx, config)
      if (cleanup !== undefined && cleanup !== null) {
        if (typeof cleanup !== 'function') throw new TypeError('apply must return a cleanup function or nothing')
        context.effect(() => cleanup)
      }
    },
  }
}
