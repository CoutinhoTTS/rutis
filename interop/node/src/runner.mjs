import { createRequire } from 'node:module'
import { pathToFileURL } from 'node:url'
import { Process } from './client.mjs'

const [socketPath, pluginPath] = process.argv.slice(2)

// The plugins' Service classes must come from the same Cordis instance as the
// Context, so prefer the Cordis that the (first) plugin itself resolves.
function cordisOf(entry) {
  try { return createRequire(entry).resolve('@deepseek-ai/cordis') } catch { return undefined }
}
const cordisPath = cordisOf(pluginPath)
const { Context } = await import(cordisPath ? pathToFileURL(cordisPath).href : '@deepseek-ai/cordis')
let peer
const ctx = new Context()
let fibers
let mounted = false
let closing = false
let disposing
let version = 0

// Each exported service slot is projected as a sequence of object handles.
// A handle always addresses the object it was created for; when the slot
// changes, the Rust side receives a new handle and replaces its native proxy.
// The first object of a slot uses the service name as its handle.
const slots = new Map() // name -> { methods, object, handle, generation }
const handles = new Map() // handle -> { name, object, current, released }

function read(name) {
  try { return ctx.get(name) } catch { return undefined }
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
    const object = read(name)
    if (object === slot.object) continue
    retire(slot.handle)
    slot.object = object
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

// Dispose in reverse load order, like a native composition unwinding.
function dispose() {
  return disposing ??= (async () => {
    for (const fiber of [...(fibers ?? [])].reverse()) await fiber.dispose()
  })()
}

// A mount is a group of plugins sharing one Context, so dependencies between
// them resolve natively. `args.plugins` lists [{ entry, config }] in load
// order; a single-plugin mount passes only `args.config`.
function mount(args) {
  if (fibers) throw new Error('plugins are already mounted')
  fibers = []
  for (const [name, methods] of Object.entries(args.services)) {
    if (name.includes('#')) throw new Error(`service name ${name} cannot be projected`)
    slots.set(name, { methods: new Set(methods), object: undefined, handle: null, generation: 0, version: 0 })
  }
  const plugins = args.plugins ?? [{ entry: pluginPath, config: args.config }]
  return (async () => {
    for (const { entry } of plugins) {
      const own = cordisOf(entry)
      if (cordisPath && own && own !== cordisPath) {
        throw new Error(`${entry} resolves a different Cordis (${own}) than ${pluginPath} (${cordisPath})`)
      }
    }
    for (const { entry, config } of plugins) {
      const module = await import(pathToFileURL(entry).href)
      // An `apply` export is a function plugin; otherwise use the default
      // export, which is how packaged plugins ship their Service class.
      const plugin = typeof module.apply === 'function' ? module : (module.default ?? module)
      fibers.push(ctx.plugin(plugin, config))
    }
    await Promise.all(fibers.map(fiber => fiber.await()))
    // Only this group runs in the process, and rutis cannot provide services
    // to it yet, so a dependency unresolved within the group never resolves.
    const unresolved = fibers.flatMap((fiber, index) => fiber.store ? [] : [
      `${plugins[index].entry} (${Object.keys(fiber.inject ?? {}).filter(name => ctx.get(name, false) === undefined).join(', ') || 'unknown'})`,
    ])
    if (unresolved.length) throw new Error(`native plugin dependencies are unresolved: ${unresolved.join('; ')}`)
    refresh()
    mounted = true
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
  const result = Reflect.apply(entry.object[method], entry.object, args)
  if (result instanceof Promise) result.then(refresh, refresh)
  else refresh()
  return result
}

peer = await Process.connect(socketPath, dispatch)
await peer.closed()
closing = true
await dispose()
