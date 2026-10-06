// Test a plugin without a host: give it the services it injects, call the
// services it provides, unload it.
//
//   const t = await load(plugin, { config: { city: 'Oslo' }, services: { llm } })
//   assert.equal(await t.service('weather').today(), 'sunny in Oslo')
//   await t.unload()
//
// It checks what a host would: the plugin uses only the services it declares
// in `inject`, provides what it declares in `provides` with every declared
// method, and runs its cleanups on unload. With `strict` (the default),
// values cross between the plugin and the test as they would between
// processes: data is copied, functions and objects with behaviour pass by
// reference, sync methods return values and async ones promises; what
// works only in one process fails here too.

import { PLUGIN_API, isPlugin } from './index.mjs'

export class PluginTestError extends Error {
  name = 'PluginTestError'
}

const builtins = [Date, RegExp, Map, Set, WeakMap, WeakSet, ArrayBuffer, DataView, Error, Promise]

// Objects with behaviour cross by reference: class instances and objects
// with methods. Everything else is data.
function hasBehaviour(value) {
  if (value === null || typeof value !== 'object' || Array.isArray(value) || ArrayBuffer.isView(value)) return false
  if (builtins.some(type => value instanceof type)) return false
  const prototype = Object.getPrototypeOf(value)
  if (prototype !== Object.prototype && prototype !== null) return true
  return Object.values(value).some(item => typeof item === 'function')
}

// `value` as the other process sees it.
function cross(value, where, seen = new Set()) {
  if (value === undefined || value === null) return value
  if (typeof value === 'symbol' || typeof value === 'bigint') {
    throw new PluginTestError(`${where}: a ${typeof value} cannot cross between processes`)
  }
  if (typeof value === 'function') return (...args) => crossResult(value(...args.map((arg, i) => cross(arg, `${where} argument ${i}`))), where)
  if (value instanceof Promise) return value.then(result => cross(result, where))
  if (typeof value !== 'object') return value
  if (hasBehaviour(value)) return reference(value, where)
  if (value instanceof Error) return Object.assign(new Error(value.message), { name: value.name })
  if (seen.has(value)) throw new PluginTestError(`${where}: cyclic data cannot cross between processes`)
  seen.add(value)
  try {
    if (Array.isArray(value)) return value.map((item, i) => cross(item, `${where}[${i}]`, seen))
    if (builtins.some(type => value instanceof type) || ArrayBuffer.isView(value)) return JSON.parse(JSON.stringify(value))
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, cross(item, `${where}.${key}`, seen)]))
  } finally {
    seen.delete(value)
  }
}

function crossResult(result, where) {
  return result instanceof Promise ? result.then(value => cross(value, where)) : cross(result, where)
}

// An object with behaviour, used from the other process: its methods are
// called by reference.
function reference(target, where) {
  return new Proxy(target, {
    get(object, property) {
      const value = Reflect.get(object, property, object)
      return typeof value === 'function' ? cross(value.bind(object), `${where}.${String(property)}`) : cross(value, `${where}.${String(property)}`)
    },
  })
}

// A service seen through its declared shape: only declared methods, sync
// ones returning values, async ones promises.
function shaped(name, object, shape, strict) {
  const service = {}
  for (const [method, kind] of Object.entries(shape)) {
    service[method] = (...args) => {
      const where = `${name}.${method}`
      const crossed = strict ? args.map((arg, i) => cross(arg, `${where} argument ${i}`)) : args
      const result = object[method](...crossed)
      if (kind === 'sync') {
        if (result instanceof Promise) throw new PluginTestError(`${where} is declared sync but returned a promise: declare it 'async'`)
        return strict ? cross(result, where) : result
      }
      return Promise.resolve(result).then(value => (strict ? cross(value, where) : value))
    }
  }
  return service
}

/**
 * Load `plugin` (what definePlugin returns, or a module whose default export
 * it is) with `config` and the services in `services`.
 */
export async function load(plugin, { config = {}, services = {}, strict = true } = {}) {
  plugin = isPlugin(plugin) ? plugin : plugin?.default
  if (!isPlugin(plugin)) throw new PluginTestError('load needs a plugin made with definePlugin')
  if (plugin.api > PLUGIN_API) {
    throw new PluginTestError(`the plugin needs plugin API ${plugin.api}; this SDK supports ${PLUGIN_API}`)
  }
  for (const name of plugin.inject) {
    if (!(name in services)) throw new PluginTestError(`the plugin injects ${name}: give the test a service ${name}`)
  }
  const provided = new Map()
  const cleanups = []
  let unloaded = false
  const ctx = {
    use(name) {
      if (!plugin.inject.includes(name)) throw new PluginTestError(`the plugin uses ${name} without declaring it in inject`)
      return strict ? cross(services[name], `service ${name}`) : services[name]
    },
    provide(name, value) {
      const shape = plugin.provides[name]
      if (shape) {
        for (const method of Object.keys(shape)) {
          if (typeof value?.[method] !== 'function') throw new PluginTestError(`${name} is declared with ${method} in provides, but the service has no such method`)
        }
      }
      const entry = { value }
      provided.set(name, entry)
      return () => {
        if (provided.get(name) === entry) provided.delete(name)
      }
    },
    effect(cleanup) {
      if (typeof cleanup !== 'function') throw new PluginTestError('effect needs a cleanup function')
      cleanups.push(cleanup)
    },
  }
  const returned = await plugin.apply(ctx, strict ? cross(config, 'config') : config)
  if (returned !== undefined && returned !== null) {
    if (typeof returned !== 'function') throw new PluginTestError('apply must return a cleanup function or nothing')
    cleanups.push(returned)
  }
  return {
    /** The service `name` the plugin provides, as rutis sees it: its declared methods only. */
    service(name) {
      if (unloaded) throw new PluginTestError('the plugin is unloaded')
      const shape = plugin.provides[name]
      if (!shape) throw new PluginTestError(`${name} is not declared in provides, so rutis cannot use it`)
      const entry = provided.get(name)
      if (!entry) throw new PluginTestError(`the plugin does not provide ${name} (now)`)
      return shaped(name, entry.value, shape, strict)
    },
    /** The names of the services the plugin provides now. */
    provided: () => [...provided.keys()],
    /** Run the cleanups, latest first; the plugin's services are withdrawn. */
    async unload() {
      if (unloaded) return
      unloaded = true
      for (const cleanup of cleanups.reverse()) await cleanup()
      provided.clear()
    },
  }
}
