import { readFileSync } from 'node:fs'
import { Connector } from './connector.mjs'
import { ENDPOINT_PROTOCOL } from '../session.mjs'

// A link from a Cordis application, as a full node: the connection and
// session with one far end, kept up. Once the far end greets as expected
// and declares what the link requires, the link provides the peer as the
// Cordis service `rutisPeer.<id>`; when the session ends it withdraws it, so
// the bridge features injecting it stop. A dialing link redials with backoff
// by failure category; a listening link lets a newer connection take over,
// ending the old session (and its features) first.

export const peerService = id => `rutisPeer.${id}`

const RETRY = { initial: 500, max: 30_000, jitter: 0.2, stable: 60_000, rejected: 30_000 }

// The families registered on a session, and the far end's offers. Each
// family offered carries the version at which it was registered (`since`):
// one registered anew is a new offer even if its withdrawal went unseen.
class Operations {
  handlers = new Map()
  since = new Map()
  remote = { families: new Set(), version: 0, since: new Map() }
  listeners = new Set()
  version = 0
  session

  dispatch = (target, method, args) => {
    if (target === '' && method === 'link.offers') {
      const [offers] = args
      if (offers?.version > this.remote.version) {
        this.remote = {
          families: new Set(offers.families),
          version: offers.version,
          since: new Map(Object.entries(offers.since ?? {})),
        }
        for (const listener of this.listeners) listener(this.remote.families, epochOf(this.remote))
      }
      return
    }
    const family = target === '' ? method.split('.')[0] : target.split(':')[0]
    const handler = this.handlers.get(family)
    if (!handler) throw new Error(`${target || method} is not offered here`)
    return handler(target, method, args)
  }

  announce() {
    if (!this.session) return
    const families = [...this.handlers.keys()]
    const since = Object.fromEntries(families.map(family => [family, this.since.get(family) ?? 0]))
    this.session.invokeAsync('', 'link.offers', [{ families, version: ++this.version, since }]).catch(() => {})
  }
}

// Which offer of a family this is, if it is offered (0 from an end that
// does not say).
const epochOf = remote => family => remote.families.has(family) ? (remote.since.get(family) ?? 0) : undefined

// What `rutisPeer.<id>` is: the far end, its session and its generation.
function peerOf(id, session, generation, operations) {
  return {
    id,
    generation,
    call: (target, method, args) => session.invoke(target, method, args),
    callAsync: (target, method, args) => session.invokeAsync(target, method, args),
    supports: capability => session.supports(capability),
    get offers() { return operations.remote.families },
    // Serve `family` on this session; returns its unregistration.
    register(family, handler) {
      if (!family || family === 'link' || /[.:]/.test(family)) throw new Error(`${family} cannot be registered`)
      if (operations.handlers.has(family)) throw new Error(`${family} is already registered`)
      operations.handlers.set(family, handler)
      operations.since.set(family, ++operations.version)
      operations.announce()
      return () => {
        if (operations.handlers.get(family) !== handler) return
        operations.handlers.delete(family)
        operations.since.delete(family)
        operations.announce()
      }
    },
    // Follow the far end's offers: `listener(families, epoch)`, where
    // `epoch(family)` says which offer of it this is. Returns the
    // unsubscription.
    onOffers(listener) {
      operations.listeners.add(listener)
      listener(operations.remote.families, epochOf(operations.remote))
      return () => operations.listeners.delete(listener)
    },
  }
}

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))

export const Link = {
  name: 'rutis-bridge/link',
  reusable: true,
  // { peer, id, dial | listen, token, ca, cert, key, require, declare, retry }
  apply(ctx, config) {
    const { peer, id, dial, listen, token, require: required = [], declare = [] } = config
    if (!!dial === !!listen) throw new Error(`the link to ${peer} needs one of dial and listen`)
    const retry = { ...RETRY, ...config.retry }
    const read = path => path ? readFileSync(path) : undefined
    const options = { protocol: `rutis.${ENDPOINT_PROTOCOL}`, token, ca: read(config.ca), cert: read(config.cert), key: read(config.key) }
    const endpoint = { local: id, expected: peer, declare: ['node', ...declare] }
    const status = { state: 'connecting', generation: 0 }
    let stopped = false, generation = 0, current, connector

    // Start a session on an accepted connection; resolves once it is ready
    // and the peer is provided, or rejects with a categorized error.
    async function start(connection) {
      const operations = new Operations()
      const session = connector.activate(connection, { dispatch: operations.dispatch, endpoint })
      await session.ready
      const missing = required.find(capability => !session.supports(capability))
      if (missing) throw Object.assign(new Error(`${peer} does not declare ${missing}`), { category: 'incompatible' })
      operations.session = session
      operations.announce()
      const live = { connection, session, since: Date.now(), generation: ++generation }
      live.withdraw = ctx.provide(peerService(peer), peerOf(peer, session, live.generation, operations))
      Object.assign(status, { state: 'ready', generation: live.generation })
      return live
    }

    // Withdraw the peer and let the features that injected it stop.
    async function end(live) {
      live.withdraw?.()
      live.withdraw = undefined
      await sleep(20)
    }

    ctx.effect(() => () => {
      stopped = true
      if (current) end(current)
      connector?.stop()
    })

    if (dial) {
      (async () => {
        let backoff = retry.initial
        while (!stopped) {
          status.state = 'connecting'
          // The connector's events go to whichever wait is current.
          const hooks = {}
          connector = new Connector({
            spec: dial, mode: 'dial', options,
            events: {
              accepted: connection => hooks.accepted?.(connection),
              failed: error => hooks.failed?.(error),
              closed: (_, reason) => hooks.closed?.(reason),
              fatal: reason => hooks.failed?.(new Error(reason)),
            },
          })
          const first = await new Promise(resolve => {
            hooks.accepted = connection => resolve({ connection })
            hooks.failed = error => resolve({ error })
            hooks.closed = reason => resolve({ error: new Error(reason) })
          })
          let failure
          if (first.connection !== undefined) {
            const ended = new Promise(resolve => {
              hooks.closed = reason => resolve(reason)
              hooks.failed = error => resolve(error.message)
            })
            try {
              current = await start(first.connection)
              await ended
              if (Date.now() - current.since >= retry.stable) backoff = retry.initial
              await end(current)
              current = undefined
              failure = { category: 'retryable', message: 'the session ended' }
            } catch (error) {
              failure = { category: error.category ?? 'retryable', message: error.message }
            }
          } else {
            failure = { category: first.error.category ?? 'retryable', message: first.error.message }
          }
          await connector.stop()
          if (stopped) return
          if (failure.category === 'incompatible') { Object.assign(status, { state: 'stopped', error: failure.message }); return }
          const wait = failure.category === 'auth-rejected'
            ? retry.rejected
            : backoff * (1 + retry.jitter * (2 * Math.random() - 1))
          if (failure.category !== 'auth-rejected') backoff = Math.min(backoff * 2, retry.max)
          Object.assign(status, { state: 'waiting', category: failure.category, error: failure.message })
          await sleep(wait)
        }
      })()
    } else {
      let switching = Promise.resolve()
      connector = new Connector({
        spec: listen, mode: 'listen', options,
        events: {
          listening: url => process.stderr.write(`rutis-interop: listening on ${url}\n`),
          accepted: connection => {
            switching = switching.then(async () => {
              if (stopped) return
              // A newer connection takes over: the old session and its
              // features end first.
              if (current) {
                const old = current
                current = undefined
                connector.replace(old.connection)
                await end(old)
              }
              try { current = await start(connection) }
              catch (error) {
                connector.close(connection, error.message)
                Object.assign(status, { state: 'waiting', category: error.category ?? 'retryable', error: error.message })
              }
            })
          },
          closed: connection => {
            switching = switching.then(async () => {
              if (current?.connection !== connection) return
              const old = current
              current = undefined
              await end(old)
              status.state = 'connecting'
            })
          },
          failed: error => Object.assign(status, { state: 'stopped', error: error.message }),
        },
      })
    }

    ctx.provide(`rutisLink.${peer}`, status)
  },
}
