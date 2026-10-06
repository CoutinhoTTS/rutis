// Write a rutis plugin in JavaScript or TypeScript. A plugin declares the
// services it uses (`inject`) and provides (`provides`), and `apply(ctx,
// config)` uses and provides them through `ctx`; rutis decides when it
// starts, stops and restarts. The runtime that runs it is installed by the
// host (`@arcships/rutis-runtime`), not by the plugin.
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

// The plugin API this SDK writes plugins against. A runtime refuses a plugin
// that needs a newer one, naming both.
export const PLUGIN_API = 1

// Marks what definePlugin returns, across copies of this package.
export const PLUGIN = Symbol.for('rutis.leaf-plugin')

export function definePlugin(spec) {
  if (typeof spec?.apply !== 'function') throw new TypeError('a plugin needs apply(ctx, config)')
  const inject = spec.inject ?? []
  if (!Array.isArray(inject) || inject.some(name => typeof name !== 'string')) throw new TypeError('inject must be a list of service names')
  for (const [name, methods] of Object.entries(spec.provides ?? {})) {
    for (const [method, kind] of Object.entries(methods ?? {})) {
      if (kind !== 'sync' && kind !== 'async') throw new TypeError(`${name}.${method}: kind must be 'sync' or 'async'`)
    }
  }
  return Object.freeze({ ...spec, inject, provides: spec.provides ?? {}, api: PLUGIN_API, [PLUGIN]: true })
}

// Whether `value` is a plugin definePlugin made.
export const isPlugin = value => value?.[PLUGIN] === true
