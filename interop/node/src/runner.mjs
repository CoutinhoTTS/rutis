import { Context } from '@deepseek-ai/cordis'
import { createConnection } from 'node:net'
import { createInterface } from 'node:readline'
import { pathToFileURL } from 'node:url'
import { encode } from './wire.mjs'

const [socketPath, pluginPath] = process.argv.slice(2)
const socket = createConnection(socketPath)
const ctx = new Context()
let pluginFiber
let exporterFiber
let services
let methods = new Map()
let closing = false
const active = new Set()
let disposing

function dispose() {
  return disposing ??= (async () => {
    if (exporterFiber) await exporterFiber.dispose()
    if (pluginFiber) await pluginFiber.dispose()
  })()
}

async function dispatch({ target, method, args }) {
  if (closing) throw new Error('plugin is closing')
  if (target === '' && method === 'mount') {
    if (pluginFiber) throw new Error('plugin is already mounted')
    const plugin = await import(pathToFileURL(pluginPath).href)
    pluginFiber = ctx.plugin(plugin, args.config)
    await pluginFiber.await()
    if (!pluginFiber.store) throw new Error('native plugin dependencies are unresolved')
    methods = new Map(Object.entries(args.services).map(([name, methods]) => [name, new Set(methods)]))
    exporterFiber = ctx.plugin({
      name: 'interop-export', inject: [...methods.keys()],
      apply(scope) {
        // This mount exports these objects, even if their service slots change.
        services ??= new Map([...methods.keys()].map(name => [name, scope[name]]))
      },
    })
    await exporterFiber.await()
    if (!services) throw new Error('declared services are unavailable')
    return null
  }
  if (target === '' && method === 'dispose') {
    closing = true
    // A disposer may supply the signal that an earlier call is awaiting.
    await Promise.all([dispose(), Promise.allSettled([...active])])
    return null
  }
  if (!methods.get(target)?.has(method)) throw new Error(`unknown service method ${target}.${method}`)
  if (!Array.isArray(args)) throw new TypeError('method arguments must be an array')
  const service = services.get(target)
  return await Reflect.apply(service[method], service, args)
}

function respond(request) {
  // Begin dispatch in the next microtask so active contains every prior call.
  const task = Promise.resolve().then(() => dispatch(request)).then(
    value => encode({ id: request.id, status: 'ok', value: value === undefined ? null : value }),
  ).catch(error => encode({ id: request.id, status: 'error', name: error?.name ?? 'Error', message: String(error?.message ?? error) }))
    .then(frame => { if (!socket.destroyed) socket.write(frame) })
  // The disposal request waits for earlier calls, never for itself.
  if (!(request.target === '' && request.method === 'dispose')) active.add(task)
  void task.finally(() => active.delete(task)).catch(() => {})
}

const lines = createInterface({ input: socket })
lines.on('line', line => {
  try {
    const request = JSON.parse(line)
    if (!Number.isSafeInteger(request.id) || typeof request.target !== 'string' || typeof request.method !== 'string') throw new Error('invalid request')
    respond(request)
  } catch (error) { socket.destroy(error) }
})
socket.on('error', error => { console.error(error.message); process.exitCode = 1 })
socket.on('close', () => {
  closing = true
  void Promise.all([dispose(), Promise.allSettled([...active])]).catch(error => { console.error(error); process.exitCode = 1 })
})
