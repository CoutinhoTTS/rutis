import { ConnectError } from './errors.mjs'
import { ENDPOINT_PROTOCOL } from '../session.mjs'
import * as fd from './fd.mjs'
import * as unix from './unix.mjs'
import * as websocket from './websocket.mjs'

export { ConnectError }

// Network sessions speak the endpoint format; its version is the
// WebSocket subprotocol.
const PROTOCOL = `rutis.${ENDPOINT_PROTOCOL}`

// open(spec, { message(text), closed(reason) }) → { send(text), end(), close(reason) }
// Specs:
//   `unix:<path>` or a bare path   dial a Unix socket
//   `fd:<n>`                       an inherited socket
//   `ws://…`, `wss://…`            dial a WebSocket endpoint
//   `listen:ws://…`, `listen:wss://…`
//                                  listen, and take the first connection
//                                  that authenticates
// Credentials come from the environment (RUTIS_TOKEN, _CA, _CERT,
// _KEY), never from the spec. Failing to connect throws a ConnectError; a
// later end is reported through `closed`.
export async function open(spec, handlers, options = websocket.optionsFromEnvironment()) {
  const scheme = /^([a-z][a-z0-9+.-]*):/.exec(spec)?.[1]
  if (scheme === undefined || scheme === 'unix') return unix.open(spec, handlers)
  if (scheme === 'fd') return fd.open(spec, handlers)
  if (scheme === 'ws' || scheme === 'wss') return websocket.open(spec, handlers, { protocol: PROTOCOL, ...options })
  if (scheme === 'listen') return listenOnce(spec.slice('listen:'.length), handlers, options)
  throw new ConnectError('incompatible', `no channel for ${scheme}: addresses`)
}

async function listenOnce(spec, handlers, options) {
  let take, taken = false, connections = 0
  const first = new Promise(resolve => { take = resolve })
  const ignored = { message() {}, closed() {} }
  const listener = await websocket.listen(spec, {
    // Only the first connection is the session; others must not end it.
    handlers: () => connections++ === 0 ? handlers : ignored,
    accepted: channel => {
      // One session: whatever else got through meanwhile is turned away.
      if (taken) return channel.close('this endpoint serves one session')
      taken = true
      take(channel)
    },
  }, { protocol: PROTOCOL, ...options })
  process.stderr.write(`rutis: listening on ${listener.url}\n`)
  const channel = await first
  listener.stop()
  return channel
}
