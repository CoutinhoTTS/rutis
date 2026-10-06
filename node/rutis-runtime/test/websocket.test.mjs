import { test } from 'node:test'
import assert from 'node:assert/strict'
import { open, listen } from '../src/channel/websocket.mjs'
import { ConnectError } from '../src/channel/errors.mjs'

const PROTOCOL = 'rutis.2'
const quick = { maxMessage: 1024, ping: 50, timeout: 200, handshake: 2000 }

function inbox() {
  const messages = []
  let wake, ended
  const closedWith = new Promise(resolve => { ended = resolve })
  return {
    handlers: { message: text => { messages.push(text); wake?.() }, closed: reason => ended(reason) },
    next: () => messages.length ? Promise.resolve(messages.shift()) : new Promise(resolve => { wake = () => { wake = undefined; resolve(messages.shift()) } }),
    closedWith,
  }
}

async function pair(limits) {
  const server = inbox(), client = inbox()
  let accept
  const accepted = new Promise(resolve => { accept = resolve })
  const listener = await listen('ws://127.0.0.1:0/rutis', { handlers: () => server.handlers, accepted: channel => accept(channel) }, { protocol: PROTOCOL, token: 'secret', limits })
  const dialed = await open(listener.url, client.handlers, { protocol: PROTOCOL, token: 'secret', limits })
  return { listener, dialed, accepted: await accepted, server, client }
}

test('messages cross both ways and an orderly close is a normal end', async () => {
  const { listener, dialed, accepted, server, client } = await pair()
  dialed.send('{"a":1}')
  assert.equal(await server.next(), '{"a":1}')
  accepted.send('{"b":"x\\ny"}')
  assert.equal(await client.next(), '{"b":"x\\ny"}')
  accepted.close('bye')
  assert.equal(await client.closedWith, undefined)
  listener.close()
})

test('a takeover closes with 4002, read as replaced', async () => {
  const { listener, accepted, client } = await pair()
  accepted.replaced()
  assert.equal(await client.closedWith, 'replaced by a new connection')
  listener.close()
})

test('authentication, subprotocol and address failures carry categories', async () => {
  const listener = await listen('ws://127.0.0.1:0/rutis', { handlers: () => inbox().handlers, accepted: () => {} }, { protocol: PROTOCOL, token: 'secret' })
  const category = async options => {
    try { await open(listener.url, inbox().handlers, options); return 'opened' }
    catch (error) { assert.ok(error instanceof ConnectError, error); return error.category }
  }
  assert.equal(await category({ protocol: PROTOCOL, token: 'guess' }), 'auth-rejected')
  assert.equal(await category({ protocol: PROTOCOL }), 'auth-rejected')
  assert.equal(await category({ protocol: 'rutis.99', token: 'secret' }), 'incompatible')
  await assert.rejects(open('ws://example.com/rutis', inbox().handlers, { protocol: PROTOCOL }), error => error.category === 'incompatible')
  const free = listener.url.replace(/:\d+\//, ':1/')
  await assert.rejects(open(free, inbox().handlers, { protocol: PROTOCOL, token: 'secret' }), error => error.category === 'retryable')
  listener.close()
})

test('a message over the limit closes the channel', async () => {
  const { listener, dialed, client, server } = await pair(quick)
  dialed.send('x'.repeat(2048))
  assert.match(await client.closedWith, /1009/)
  await server.closedWith
  listener.close()
})

// A TCP relay that can stop carrying bytes without closing: a half-open
// connection, as a vanished network leaves it.
async function freezableRelay(target) {
  const { createServer, connect } = await import('node:net')
  const state = { frozen: false }
  const relay = createServer(client => {
    const upstream = connect(Number(target.port), target.hostname)
    for (const [from, to] of [[client, upstream], [upstream, client]]) {
      from.on('data', data => { if (!state.frozen) to.write(data) })
      from.on('error', () => {})
    }
  })
  await new Promise(resolve => relay.listen(0, '127.0.0.1', resolve))
  return { port: relay.address().port, state, close: () => relay.close() }
}

test('heartbeats find a half-open connection on both sides', async () => {
  const server = inbox(), client = inbox()
  let accept
  const accepted = new Promise(resolve => { accept = resolve })
  const listener = await listen('ws://127.0.0.1:0/rutis', { handlers: () => server.handlers, accepted: channel => accept(channel) }, { protocol: PROTOCOL, token: 'secret', limits: quick })
  const relay = await freezableRelay(new URL(listener.url))
  const dialed = await open(`ws://127.0.0.1:${relay.port}/rutis`, client.handlers, { protocol: PROTOCOL, token: 'secret', limits: quick })
  await accepted
  // Alive while pings flow, for longer than the timeout.
  await new Promise(resolve => setTimeout(resolve, 500))
  dialed.send('{"still":"here"}')
  assert.equal(await server.next(), '{"still":"here"}')
  relay.state.frozen = true
  const started = Date.now()
  assert.match(await client.closedWith, /heartbeat timeout/)
  assert.match(await server.closedWith, /heartbeat timeout/)
  assert.ok(Date.now() - started < 2000)
  relay.close()
  listener.close()
})
