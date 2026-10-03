import { createRequire } from 'node:module'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { Process } from './client.mjs'
import { toJsonSchema } from './schema.mjs'

const [socketPath, pluginPath] = process.argv.slice(2)

// The plugins' Service classes must come from the same Cordis instance as the
// Context, so prefer the Cordis that the (first) plugin itself resolves.
function cordisOf(entry) {
  try { return createRequire(entry).resolve('@deepseek-ai/cordis') } catch { return undefined }
}
const cordisPath = cordisOf(pluginPath)
const { Context, resolveConfig } = await import(cordisPath ? pathToFileURL(cordisPath).href : '@deepseek-ai/cordis')
// Volatile config helpers from the cosmokit that Cordis itself uses.
const cosmokit = await import(pathToFileURL(createRequire(cordisPath ?? fileURLToPath(import.meta.resolve('@deepseek-ai/cordis'))).resolve('@deepseek-ai/cosmokit')).href).catch(() => ({}))
let peer
const ctx = new Context()
let fibers
let mounted = false
let closing = false
let disposing
let version = 0
let emits = new Set() // events the rutis side may emit here
// Rows: plugins rutis-loader manages one by one in this Context (`rows.*`).
const rows = new Map() // key -> { fiber, inner, config }

// Each exported service slot is projected as a sequence of object handles.
// A handle always addresses the object it was created for; when the slot
// changes, the Rust side receives a new handle and replaces its native proxy.
// The first object of a slot uses the service name as its handle.
const slots = new Map() // name -> { methods, scope, object, identity, handle, generation, exporter }
const handles = new Map() // handle -> { name, object, current, released }

// Cordis wraps Service instances in a new tracing proxy on every read; the
// proxy reports its target under this symbol. Compare targets, not wrappers.
const ORIGINAL = Symbol.for('cordis.original')
const identity = value => (value !== null && (typeof value === 'object' || typeof value === 'function')) ? (value[ORIGINAL] ?? value) : value

// Every exported service is read through its own exporter fiber that injects
// it, like any native consumer: Cordis gates it on availability including
// Service.check(), and effects that service methods create through the
// caller's context belong to this fiber and are disposed with it.
function exporter(name, slot) {
  return ctx.plugin({
    name: `interop-export:${name}`,
    inject: [name],
    apply(scope) {
      slot.scope = scope
      refresh()
      scope.effect(() => () => {
        if (slot.scope !== scope) return
        slot.scope = undefined
        refresh()
      })
    },
  })
}

function read(slot, name) {
  if (!slot.scope) return undefined
  try { return slot.scope.get(name) } catch { return undefined }
}

function retire(handle) {
  const entry = handle && handles.get(handle)
  if (!entry) return
  entry.current = false
  if (entry.released) handles.delete(handle)
}

// Re-read every slot after any signal that may change it. Cordis emits
// internal/service for provide, withdrawal and provider activation, and
// internal/set for property assignment; a direct ctx.set() emits nothing and
// is observed after the next call into this process.
function refresh() {
  for (const [name, slot] of slots) {
    const object = read(slot, name)
    const current = identity(object)
    if (current === slot.identity) {
      // Same service, possibly read through a new exporter scope: calls must
      // use the live scope, but the handle and Rust proxy stay.
      if (object !== undefined && slot.handle) handles.get(slot.handle).object = object
      continue
    }
    retire(slot.handle)
    slot.object = object
    slot.identity = current
    slot.handle = null
    if (object !== undefined) {
      slot.generation++
      slot.handle = slot.generation === 1 ? name : `${name}#${slot.generation}`
      handles.set(slot.handle, { name, object, current: true, released: false })
    }
    slot.version = ++version
    if (mounted && !closing) {
      peer.callAsync('', 'service', [name, slot.handle, slot.version]).catch(() => {})
    }
  }
}

ctx.on('internal/service', () => { if (slots.size) refresh() })
ctx.on('internal/set', (_ctx, _name, _value, _error, next) => {
  const result = next()
  if (slots.size) refresh()
  return result
})

// Exporters go first (they consume the services), then rows and the
// plugins in reverse load order, like a native composition unwinding.
function dispose() {
  return disposing ??= (async () => {
    for (const slot of slots.values()) await slot.exporter?.dispose()
    for (const { fiber } of [...rows.values()].reverse()) await fiber.dispose()
    rows.clear()
    for (const fiber of [...(fibers ?? [])].reverse()) await fiber.dispose()
  })()
}

// A plugin module: an `apply` export is a function plugin; otherwise the
// default export, which is how packaged plugins ship their Service class.
async function pluginOf(entry) {
  const own = cordisOf(entry)
  if (cordisPath && own && own !== cordisPath) {
    throw new Error(`${entry} resolves a different Cordis (${own}) than ${pluginPath} (${cordisPath})`)
  }
  const module = await import(pathToFileURL(entry).href)
  return typeof module.apply === 'function' ? module : (module.default ?? module)
}

// `plugin()` returns a thenable `Object.create(fiber)`; state written through
// it (`update` sets `_config`) would land on the wrapper and never reach the
// fiber that later activates. Rows keep the fiber itself.
const fiberOf = wrapped => Object.hasOwn(wrapped, 'then') ? Object.getPrototypeOf(wrapped) : wrapped

// One row: `isolate` as [name, label] pairs (rows naming a label share its
// scope), `inject` as extra service names gating the row. With `inject`, the
// plugin runs inside a gate fiber (`inner`), from the row's latest config.
async function loadRow([key, entry, config, isolate, inject]) {
  if (rows.has(key)) throw new Error(`row ${key} is already loaded`)
  const plugin = await pluginOf(entry)
  let scope = ctx
  for (const [name, label] of isolate ?? []) scope = scope.isolate(name, Symbol.for(`rutis-row:${label}`))
  const row = { fiber: undefined, inner: undefined, config }
  const fiber = fiberOf(inject?.length
    ? scope.plugin({ name: `row:${key}`, inject, apply(gated) { row.inner = fiberOf(gated.plugin(plugin, row.config)) } })
    : scope.plugin(plugin, config))
  row.fiber = fiber
  if (!inject?.length) row.inner = fiber
  rows.set(key, row)
  try {
    await fiber.await()
  } catch (error) {
    rows.delete(key)
    await fiber.dispose()
    throw error
  }
  return null
}

// A new config for a row. Volatile values are committed into the running
// plugin's references and announced with `loader/volatile-update`, as
// cordis-plugin-loader's `_commitVolatile` does. Whenever that cannot apply
// the change — ordinary values changed too, or the parsed config holds no
// volatile references to commit into — the row takes an ordinary update
// (a restart), so the plugin never keeps running on the old values.
// (rutis-loader sends only changes its schema calls volatile; Cordis's parse
// of the config has the last word.)
async function updateRow([key, config]) {
  const row = rows.get(key)
  if (!row) throw new Error(`row ${key} is not loaded`)
  row.config = config
  const fiber = row.inner
  // A gate whose plugin fiber is gone re-applies from row.config.
  if (!fiber || fiber.uid === null) return null
  // Not running (waiting for its own inject, or failed): store the config
  // in the fiber, which activates from it.
  if (fiber.state !== 2) {
    fiber.update(config, true)
    return null
  }
  if (commitVolatile(fiber, config)) return null
  fiber.update(config, true)
    await fiber.await()
  return null
}

// Whether the change was committed in place. Throws when the config does
// not validate.
function commitVolatile(fiber, raw) {
  const { volatileEntries, updateVolatile, deepEqual } = cosmokit
  if (!volatileEntries || !resolveConfig) return false
  const refs = volatileEntries(fiber.config)
  if (!refs.length) return false
  const candidate = resolveConfig(fiber.runtime, fiber.ctx.waterfall(fiber, 'internal/config', raw, () => raw))
  if (!deepEqual(fiber.config, candidate, true)) return false
  fiber._config = raw
  const paths = refs.flatMap(({ path, ref }) => {
    const source = path.reduce((value, key) => Reflect.get(value, key), candidate)
    if (deepEqual(ref.get(), source.get(), true)) return []
    updateVolatile(ref, source)
    return [path]
  })
  if (paths.length) {
    const self = Object.create(fiber.ctx)
    // Only the row's own fiber hears it.
    self[Context.filter] = owner => owner.fiber === fiber
    fiber.ctx.emit(self, 'loader/volatile-update', paths)
  }
  return true
}

// A rutis service seen from Cordis: bound methods call the Rust host
// (synchronously or returning a Promise, as declared); any other method is
// reported as not provided rather than silently missing. It is not an
// instance of the class the plugin declares (boundary rule 7).
function hostProxy(name, methods) {
  const target = {}
  for (const [method, kind] of Object.entries(methods)) {
    target[method] = kind === 'async'
      ? (...args) => peer.callAsync(`host:${name}`, method, args)
      : (...args) => peer.call(`host:${name}`, method, args)
  }
  const passthrough = new Set(['then', 'toJSON', 'constructor'])
  return new Proxy(target, {
    get(target, property, receiver) {
      if (typeof property !== 'string' || property in target || passthrough.has(property)) return Reflect.get(target, property, receiver)
      return () => { throw new Error(`${name}.${property} is not provided by the rutis host`) }
    },
  })
}

// A mount is a group of plugins sharing one Context, so dependencies between
// them resolve natively. `args.plugins` lists [{ entry, config }] in load
// order; a single-plugin mount passes only `args.config`.
function mount(args) {
  if (fibers) throw new Error('plugins are already mounted')
  fibers = []
  for (const [name, methods] of Object.entries(args.services ?? {})) {
    if (name.includes('#')) throw new Error(`service name ${name} cannot be projected`)
    slots.set(name, { methods: new Set(methods), scope: undefined, object: undefined, identity: undefined, handle: null, generation: 0, version: 0 })
  }
  const plugins = args.plugins ?? [{ entry: pluginPath, config: args.config }]
  emits = new Set(args.emits ?? [])
  // Services the rutis application provides are registered before the
  // plugins load, so the plugins' dependencies on them resolve natively.
  for (const [name, methods] of Object.entries(args.provided ?? {})) ctx.provide(name, hostProxy(name, methods))
  return (async () => {
    for (const { entry } of plugins) {
      const own = cordisOf(entry)
      if (cordisPath && own && own !== cordisPath) {
        throw new Error(`${entry} resolves a different Cordis (${own}) than ${pluginPath} (${cordisPath})`)
      }
    }
    for (const { entry, config } of plugins) {
      fibers.push(ctx.plugin(await pluginOf(entry), config))
    }
    for (const [name, slot] of slots) slot.exporter = exporter(name, slot)
    await Promise.all([...fibers, ...[...slots.values()].map(slot => slot.exporter)].map(fiber => fiber.await()))
    // Only this group runs in the process, and rutis cannot provide services
    // to it yet, so a dependency unresolved within the group never resolves.
    const unresolved = fibers.flatMap((fiber, index) => fiber.store ? [] : [
      `${plugins[index].entry} (${Object.keys(fiber.inject ?? {}).filter(name => ctx.get(name, false) === undefined).join(', ') || 'unknown'})`,
    ])
    if (unresolved.length) throw new Error(`native plugin dependencies are unresolved: ${unresolved.join('; ')}`)
    refresh()
    mounted = true
    // Forward selected Cordis events to rutis. Registered after the plugins
    // started, so the rutis side runs as one group after the listeners the
    // plugins registered while starting. emit ignores the returned Promise
    // (fire and forget); parallel / serial wait for the rutis listeners.
    for (const name of args.events ?? []) {
      ctx.on(name, (...values) => {
        const done = peer.callAsync('', 'event', [name, values])
        done.catch(() => {}) // an ignored emit must not become an unhandled rejection
        return done
      })
    }
    return { services: Object.fromEntries([...slots].map(([name, slot]) => [name, [slot.handle, slot.version]])) }
  })()
}

function dispatch(target, method, args) {
  if (closing) throw new Error('plugin is closing')
  if (target === '') {
    switch (method) {
      case 'mount': return mount(args)
      case 'dispose':
        closing = true
        return Promise.all([dispose(), peer.drain()]).then(() => null)
      case 'emit': {
        // A rutis event re-emitted for the Cordis listeners.
        const [name, values] = args ?? []
        if (!emits.has(name)) throw new Error(`event ${name} is not declared for emission from rutis`)
        return ctx.parallel(name, ...(values ?? []))
      }
      case 'get': {
        // A live read of a declared service property.
        const [handle, property] = args ?? []
        const entry = handles.get(handle)
        if (!entry) throw new Error(`unknown or released service object ${handle}`)
        if (!slots.get(entry.name).methods.has(property)) throw new Error(`unknown service property ${entry.name}.${property}`)
        return entry.object[property]
      }
      case 'rows.load': return loadRow(args ?? [])
      case 'rows.update': return updateRow(args ?? [])
      case 'rows.unload': {
        const row = rows.get(args?.[0])
        rows.delete(args?.[0])
        return row ? row.fiber.dispose().then(() => null) : null
      }
      case 'rows.schema': return pluginOf(args?.[0]).then(plugin => {
        const schema = plugin.Config ?? plugin.schema
        return schema ? toJsonSchema(schema) : null
      })
      case 'release': {
        const entry = handles.get(args?.[0])
        if (entry) { entry.released = true; if (!entry.current) handles.delete(args[0]) }
        return null
      }
      default: throw new Error(`unknown control method ${method}`)
    }
  }
  const entry = handles.get(target)
  if (!entry) throw new Error(`unknown or released service object ${target}`)
  if (!slots.get(entry.name).methods.has(method)) throw new Error(`unknown service method ${entry.name}.${method}`)
  if (!Array.isArray(args)) throw new TypeError('method arguments must be an array')
  let result
  try {
    result = Reflect.apply(entry.object[method], entry.object, args)
  } finally {
    // Also after a throw: the method may have replaced the service first.
    if (!(result instanceof Promise)) refresh()
  }
  if (result instanceof Promise) result.then(refresh, refresh)
  return result
}

// Calls on exported objects and functions may replace services too.
peer = await Process.connect(socketPath, dispatch, () => { if (slots.size && !closing) refresh() })
await peer.closed()
closing = true
await dispose()
