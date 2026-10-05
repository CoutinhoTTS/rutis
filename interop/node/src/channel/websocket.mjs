import { readFileSync } from 'node:fs'
import { createServer as createHttpServer } from 'node:http'
import { createServer as createHttpsServer } from 'node:https'
import { timingSafeEqual } from 'node:crypto'
import { isIP } from 'node:net'
import { WebSocket, WebSocketServer } from 'ws'
import { ConnectError } from './errors.mjs'

// The WebSocket binding (docs/design-protocol-channel-decoupling-2026-10-03.md):
// one connection carries one channel of UTF-8 JSON text messages; the
// session protocol is the subprotocol; bearer tokens travel in the
// Authorization header, never in the URL; both sides ping every 10 s and
// drop a far end silent for 30 s; 16 MiB per message, 1009 over it.

export const LIMITS = { maxMessage: 16 * 1024 * 1024, ping: 10_000, timeout: 30_000, handshake: 10_000 }
const GOING_AWAY = 1001, TOO_BIG = 1009, REPLACED = 4002

const truncate = reason => {
  let bytes = Buffer.from(reason ?? '')
  if (bytes.length <= 123) return reason ?? ''
  let end = 123
  while (end > 0 && (bytes[end] & 0xc0) === 0x80) end--
  return bytes.subarray(0, end).toString()
}

const loopback = host => {
  const bare = host.replace(/^\[|\]$/g, '')
  return bare === 'localhost' || (isIP(bare) === 4 && bare.startsWith('127.')) || bare === '::1'
}

// A channel over an open WebSocket: { send(text), end(), close(reason), replaced() }.
function wrap(socket, { message, closed }, limits) {
  let ended = false, failure
  let lastSeen = Date.now()
  const finish = reason => {
    if (ended) return
    ended = true
    clearInterval(heartbeat)
    closed(reason)
  }
  const heartbeat = setInterval(() => {
    if (Date.now() - lastSeen > limits.timeout) {
      failure = `no message from the far end for ${limits.timeout / 1000} s: heartbeat timeout`
      socket.terminate()
      return
    }
    if (socket.readyState === WebSocket.OPEN) socket.ping()
  }, limits.ping)
  heartbeat.unref?.()
  const seen = () => { lastSeen = Date.now() }
  socket.on('ping', seen)
  socket.on('pong', seen)
  socket.on('message', (data, binary) => {
    seen()
    if (binary) {
      failure = 'binary messages are reserved for a binary encoding'
      socket.close(1003, failure)
      return
    }
    message(data.toString('utf8'))
  })
  socket.on('error', error => { failure ??= error.code === 'WS_ERR_UNSUPPORTED_MESSAGE_LENGTH' ? `received a message over the limit (${TOO_BIG})` : error.message })
  socket.on('close', (code, reason) => {
    if (failure === undefined) {
      if (code === REPLACED) failure = 'replaced by a new connection'
      else if (code !== 1000 && code !== GOING_AWAY && code !== 1005) failure = `closed by the far end (${code}): ${reason}`
    }
    finish(failure)
  })
  return {
    send(text) {
      if (ended) return
      if (Buffer.byteLength(text) > limits.maxMessage) {
        failure = `message over the limit (${TOO_BIG})`
        socket.close(TOO_BIG, failure)
        return
      }
      socket.send(text)
    },
    end() { socket.close(GOING_AWAY, '') },
    close(reason) { failure ??= reason; socket.close(GOING_AWAY, truncate(reason)) },
    replaced() { failure ??= 'replaced by a new connection'; socket.close(REPLACED, 'replaced by a new connection') },
  }
}

// What this side presents and trusts, from the environment: the token it
// sends (dial) or accepts (listen), a CA for wss, and a listener's
// certificate and key.
export function optionsFromEnvironment(env = process.env) {
  const read = name => env[name] ? readFileSync(env[name]) : undefined
  return { token: env.RUTIS_INTEROP_TOKEN, ca: read('RUTIS_INTEROP_CA'), cert: read('RUTIS_INTEROP_CERT'), key: read('RUTIS_INTEROP_KEY') }
}

// Dial `spec` (ws:// or wss://) once.
export function open(spec, handlers, { protocol, token, ca, limits = LIMITS } = {}) {
  const url = new URL(spec)
  if (url.protocol === 'ws:' && !loopback(url.hostname)) {
    return Promise.reject(new ConnectError('incompatible', `${spec}: a non-loopback address needs wss://`))
  }
  return new Promise((resolve, reject) => {
    let category
    const socket = new WebSocket(spec, protocol ? [protocol] : [], {
      headers: token ? { Authorization: `Bearer ${token}` } : {},
      ca, maxPayload: limits.maxMessage, handshakeTimeout: limits.handshake,
    })
    socket.once('upgrade', response => {
      const offered = response.headers['sec-websocket-protocol']
      if (protocol && offered !== protocol) category = 'incompatible'
    })
    socket.once('unexpected-response', (_request, response) => {
      const status = response.statusCode
      category = status === 401 || status === 403 ? 'auth-rejected'
        : [400, 404, 405, 426].includes(status) ? 'incompatible' : 'retryable'
      socket.terminate()
      reject(new ConnectError(category, `${url.host}: upgrade refused with ${status}`))
    })
    socket.once('error', error => {
      const certificate = typeof error.code === 'string' && (/CERT|SIGNATURE|ALTNAME|ISSUER/.test(error.code))
      reject(new ConnectError(category ?? (certificate ? 'auth-rejected' : 'retryable'), `${url.host}: ${error.message}`))
    })
    socket.once('open', () => resolve(wrap(socket, handlers, limits)))
  })
}

const sameSecret = (a, b) => {
  const x = Buffer.from(a), y = Buffer.from(b)
  return x.length === y.length && timingSafeEqual(x, y)
}

// Listen on `spec` (ws:// or wss://, loopback only without TLS) and accept
// connections that present `token` and speak `protocol`; `accepted(channel)`
// runs for each, with the channel's handlers from `handlers()`. Resolves to
// { url, stop(), close() } once listening.
export function listen(spec, { handlers, accepted }, { protocol, token, cert, key, limits = LIMITS } = {}) {
  const url = new URL(spec)
  const secure = url.protocol === 'wss:'
  if (!secure && !loopback(url.hostname)) {
    return Promise.reject(new ConnectError('incompatible', `${spec}: only a loopback listener may go without TLS`))
  }
  if (secure && !(cert && key)) return Promise.reject(new ConnectError('incompatible', `${spec}: wss needs a certificate and key`))
  const server = secure ? createHttpsServer({ cert, key }) : createHttpServer()
  const wss = new WebSocketServer({
    server, path: url.pathname, maxPayload: limits.maxMessage,
    handleProtocols: protocols => protocols.has(protocol) ? protocol : false,
    verifyClient: ({ req }, done) => {
      const header = req.headers.authorization ?? ''
      const presented = header.startsWith('Bearer ') ? header.slice('Bearer '.length) : undefined
      if (presented === undefined) return done(false, 401, 'credentials required')
      if (!token || !sameSecret(presented, token)) return done(false, 403, 'not accepted here')
      const offered = (req.headers['sec-websocket-protocol'] ?? '').split(',').map(p => p.trim())
      if (!offered.includes(protocol)) return done(false, 400, `this endpoint speaks ${protocol}`)
      done(true)
    },
  })
  wss.on('connection', socket => accepted(wrap(socket, handlers(), limits)))
  return new Promise((resolve, reject) => {
    server.once('error', error => reject(new ConnectError('retryable', `${spec}: ${error.message}`)))
    // URL drops an explicit port 0 (pick one); read the authority itself.
    const explicit = /^[a-z]+:\/\/(?:\[[^\]]*\]|[^/:]*):(\d+)/.exec(spec)?.[1]
    const port = explicit !== undefined ? Number(explicit) : secure ? 443 : 80
    server.listen(port, url.hostname.replace(/^\[|\]$/g, ''), () => {
      const { port } = server.address()
      const bound = new URL(spec)
      bound.port = String(port)
      const stop = () => { wss.close(); server.close() }
      resolve({
        url: bound.href,
        // Stop accepting; established channels stay.
        stop,
        // Stop accepting and close every channel.
        close: () => { for (const client of wss.clients) client.close(GOING_AWAY, 'listener closed'); stop() },
      })
    })
  })
}
