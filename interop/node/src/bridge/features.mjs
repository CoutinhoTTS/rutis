import { createRequire } from 'node:module'
import { dirname, isAbsolute, join } from 'node:path'
import { readFileSync } from 'node:fs'
import { pathToFileURL } from 'node:url'
import { peerService } from './link.mjs'
import { toJsonSchema } from '../schema.mjs'
import { isLeaf, toCordis } from '../plugin.mjs'

// The node features of a Cordis application, each gated on its link's peer
// (`rutisPeer.<id>`), so each session gets them anew. They speak what the
// Rust ones do (rutis-bridge): services cross as records of functions with
// their shape, plugins are hosted by name, events go one way per name.

const fiberOf = wrapped => Object.hasOwn(wrapped, 'then') ? Object.getPrototypeOf(wrapped) : wrapped

// Each of these plugins runs a gated child: its config is static, but the
// peer it injects is named in the config.
function gated(name, peer, apply) {
  return { name, inject: [peerService(peer)], apply(ctx) { apply(ctx, ctx.get(peerService(peer))) } }
}

// export: { peer, services: { name: { method: 'sync' | 'async' } } }
// Announces the Cordis services to the peer while they are provided, and
// once the peer offers `services`; withdraws them when they go.
export const Export = {
  name: 'rutis-bridge/export',
  reusable: true,
  apply(ctx, { peer, services }) {
    ctx.plugin(gated(`export:${peer}`, peer, (scope, link) => {
      for (const [name, shape] of Object.entries(services ?? {})) {
        scope.plugin({
          name: `export:${peer}:${name}`,
          inject: [name],
          apply(one) {
            // Calls read the service when made: a live view of it.
            const record = Object.fromEntries(Object.keys(shape).map(method =>
              [method, (...args) => one.get(name)[method](...args)]))
            const announced = link.nextVersion()
            // Announced once to every offer of `services` (an importer
            // started again is a new one).
            let sentTo
            const off = link.onOffers((_families, epoch) => {
              const offered = epoch('services')
              if (offered === sentTo) return
              sentTo = offered
              if (offered === undefined) return
              link.callAsync('', 'services.announce', [{ name, service: record, shape, version: announced }])
                .catch(error => process.stderr.write(`rutis-bridge: cannot announce ${name} to ${peer}: ${error.message}\n`))
            })
            one.effect(() => async () => {
              off()
              // Withdrawn there before this ends; a peer gone has nothing to withdraw.
              await link.callAsync('', 'services.withdraw', [{ name, version: link.nextVersion() }]).catch(() => {})
            })
          },
        })
      }
    }))
  },
}

// import: { peer, services: [name] }
// Provides the peer's announced services as Cordis services; never replaces
// one provided here.
export const Import = {
  name: 'rutis-bridge/import',
  reusable: true,
  apply(ctx, { peer, services }) {
    const wanted = new Set(services ?? [])
    ctx.plugin(gated(`import:${peer}`, peer, (scope, link) => {
      // The newest the peer said of each name: provided at `version`
      // (`withdraw` set), or withdrawn at it. A withdrawal is kept so an
      // older announcement arriving after it does not bring the service back.
      const current = new Map()
      const unregister = link.register('services', (_target, method, [fields]) => {
        const { name, version } = fields ?? {}
        if (method === 'services.withdraw') {
          if (!wanted.has(name)) return
          const known = current.get(name)
          if (known && known.version >= version) return
          current.set(name, { version, withdraw: undefined })
          known?.withdraw?.()
          return
        }
        if (method !== 'services.announce') throw new Error(`${method} is not offered here`)
        if (!wanted.has(name)) return
        const known = current.get(name)
        if (known && known.version >= version) return
        if (!known?.withdraw && scope.get(name, false) !== undefined) {
          throw new Error(`${name} is already provided here: the import from ${peer} is refused`)
        }
        const { service, shape } = fields
        const object = Object.fromEntries(Object.keys(shape ?? {}).map(method => [method, (...args) => service[method](...args)]))
        known?.withdraw?.()
        current.set(name, { version, withdraw: scope.provide(name, object) })
      })
      scope.effect(() => () => {
        unregister()
        for (const { withdraw } of current.values()) withdraw?.()
        current.clear()
      })
    }))
  },
}

// The package.json above `file`, if any.
function manifestOf(file) {
  for (let dir = dirname(file); ; dir = dirname(dir)) {
    try { return JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8')) } catch {}
    if (dirname(dir) === dir) return undefined
  }
}

// host: { peer, anchor? }
// Loads the npm plugins installed here (resolved from `anchor`, a
// package.json; the working directory's by default) for the peer. What it
// loads is its own subtree. Opening a host gives the peer the management of
// what is installed here: open it only to trusted peers.
export const Host = {
  name: 'rutis-bridge/host',
  reusable: true,
  apply(ctx, { peer, anchor }) {
    const require = createRequire(anchor ?? join(process.cwd(), 'package.json'))
    async function locate(name) {
      if (typeof name !== 'string' || name.startsWith('file:') || isAbsolute(name)) {
        throw new Error(`${name}: a host loads installed plugins only, not files`)
      }
      let file
      try { file = require.resolve(name) }
      catch { throw Object.assign(new Error(`no plugin ${name} is installed here`), { name: 'NotFound' }) }
      const module = await import(pathToFileURL(file).href)
      const declared = typeof module.apply === 'function' ? module : module.default
      return { plugin: isLeaf(declared) ? toCordis(declared) : declared, declared, version: manifestOf(file)?.version ?? null }
    }
    ctx.plugin(gated(`host:${peer}`, peer, (scope, link) => {
      const loaded = new Map()
      // A hosted row runs in its scope: each [name, label] of `isolate`
      // isolated (labels are the peer's, apart from other peers'), gated on
      // the services `inject` names, as the runner runs a row.
      const start = entry => {
        const fiber = entry.inject.length
          ? entry.scope.plugin({ name: `host:${peer}:${entry.key}`, inject: entry.inject, apply(gate) { gate.plugin(entry.plugin, entry.config) } })
          : entry.scope.plugin(entry.plugin, entry.config)
        entry.fiber = fiberOf(fiber)
      }
      const unregister = link.register('plugins', async (_target, method, args) => {
        if (method === 'plugins.describe') {
          const { declared, version } = await locate(args[0])
          return { schema: declared?.Config ? toJsonSchema(declared.Config) : null, version, integrity: null }
        }
        if (method === 'plugins.load') {
          const [key, name, config, isolate, inject] = args
          if (loaded.has(key)) throw new Error(`${key} is already loaded`)
          const { plugin } = await locate(name)
          let rowScope = scope
          for (const [service, label] of isolate ?? []) rowScope = rowScope.isolate(service, Symbol.for(`rutis-peer:${peer}/${label}`))
          const entry = { key, plugin, config: config ?? {}, scope: rowScope, inject: inject ?? [], fiber: undefined }
          start(entry)
          loaded.set(key, entry)
          await entry.fiber.await()
          return null
        }
        if (method === 'plugins.update') {
          const [key, config] = args
          const entry = loaded.get(key)
          if (!entry) throw new Error(`${key} is not loaded`)
          // A new config restarts it, in the same scope.
          await entry.fiber.dispose()
          entry.config = config ?? {}
          start(entry)
          await entry.fiber.await()
          return null
        }
        if (method === 'plugins.unload') {
          const entry = loaded.get(args[0])
          loaded.delete(args[0])
          await entry?.fiber.dispose()
          return null
        }
        throw new Error(`${method} is not offered here`)
      })
      scope.effect(() => () => {
        unregister()
        loaded.clear()
      })
    }))
  },
}

// events: { peer, out: [name], in: [name] }
// Forwards `out` events to the peer and emits the `in` events it forwards
// here with `parallel`; a name goes one way only.
export const Events = {
  name: 'rutis-bridge/events',
  reusable: true,
  apply(ctx, { peer, out = [], in: inbound = [] }) {
    const both = out.find(name => inbound.includes(name))
    if (both) throw new Error(`event ${both} cannot be forwarded both ways on the link to ${peer}`)
    ctx.plugin(gated(`events:${peer}`, peer, (scope, link) => {
      for (const name of out) {
        scope.on(name, (...args) => link.callAsync('', 'events.forward', [{ name, args }]))
      }
      if (inbound.length) {
        const unregister = link.register('events', async (_target, method, [fields]) => {
          if (method !== 'events.forward') throw new Error(`${method} is not offered here`)
          if (!inbound.includes(fields?.name)) throw new Error(`event ${fields?.name} is not forwarded here`)
          const args = Array.isArray(fields.args) ? fields.args : [fields.args]
          await scope.parallel(fields.name, ...args)
          return null
        })
        scope.effect(() => unregister)
      }
    }))
  },
}
