// Leaf plugins, as the SDK (@arcships/rutis) makes them: a JavaScript or
// TypeScript plugin that needs no Cordis. The runtime loads it into its
// Cordis Context like any plugin, so it shares services with Cordis plugins
// in the same process; the plugin itself only sees `ctx.use`, `ctx.provide`
// and `ctx.effect`. The runtime does not import the SDK: it recognises its
// plugins by the symbol the SDK marks them with.

// The plugin APIs this runtime runs: a plugin written against a newer one
// is refused, naming both.
export const PLUGIN_API = 1

const PLUGIN = Symbol.for('rutis.leaf-plugin')

export const isLeaf = value => value?.[PLUGIN] === true

// Refuse a plugin that needs a newer plugin API than this runtime's.
export function supported(plugin, name) {
  const api = plugin?.api ?? 1
  if (api > PLUGIN_API) {
    throw new Error(`plugin ${name} needs plugin API ${api}; this runtime supports ${PLUGIN_API}: upgrade @arcships/rutis-runtime where the host runs`)
  }
  return plugin
}

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
