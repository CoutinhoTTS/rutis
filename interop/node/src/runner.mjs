import { Context } from '@deepseek-ai/cordis'
import { pathToFileURL } from 'node:url'
import { Process } from './client.mjs'

const [socketPath, pluginPath] = process.argv.slice(2)
let peer
const ctx = new Context()
let pluginFiber
let exporterFiber
let services
let methods = new Map()
let closing = false
let disposing

function dispose() {
  return disposing ??= (async () => {
    if (exporterFiber) await exporterFiber.dispose()
    if (pluginFiber) await pluginFiber.dispose()
  })()
}

function dispatch(target, method, args) {
  if (closing) throw new Error('plugin is closing')
  if (target === '' && method === 'mount') {
    if (pluginFiber) throw new Error('plugin is already mounted')
    return (async () => {
      const plugin = await import(pathToFileURL(pluginPath).href)
      pluginFiber = ctx.plugin(plugin, args.config)
      await pluginFiber.await()
      if (!pluginFiber.store) throw new Error('native plugin dependencies are unresolved')
      methods = new Map(Object.entries(args.services).map(([name, methods]) => [name, new Set(methods)]))
      exporterFiber = ctx.plugin({
        name: 'interop-export', inject: [...methods.keys()],
        apply(scope) { services ??= new Map([...methods.keys()].map(name => [name, scope[name]])) },
      })
      await exporterFiber.await()
      if (!services) throw new Error('declared services are unavailable')
      return null
    })()
  }
  if (target === '' && method === 'dispose') {
    closing = true
    return Promise.all([dispose(), peer.drain()]).then(() => null)
  }
  if (!methods.get(target)?.has(method)) throw new Error(`unknown service method ${target}.${method}`)
  if (!Array.isArray(args)) throw new TypeError('method arguments must be an array')
  const service = services.get(target)
  return Reflect.apply(service[method], service, args)
}

peer = await Process.connect(socketPath, dispatch)
await peer.closed()
closing = true
await dispose()
