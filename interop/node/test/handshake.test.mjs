import { test } from 'node:test'
import assert from 'node:assert/strict'
import { WebSocketServer } from 'ws'
import { Process } from '../src/client.mjs'

// The far end greets the moment it accepts, so its hello can arrive in the
// same read as the upgrade: this side still says hello before anything else.
test('a dialer greets first even when the far end greets at once', async () => {
  for (let round = 0; round < 20; round++) {
    const server = new WebSocketServer({ host: '127.0.0.1', port: 0, handleProtocols: protocols => [...protocols][0] })
    let peer
    try {
      await new Promise(resolve => server.once('listening', resolve))
      const first = new Promise(resolve => server.once('connection', socket => {
        socket.send(JSON.stringify({ op: 'hello', version: 3, endpoint: 'far', implementation: { name: 'test', version: '0' }, capabilities: [] }))
        socket.once('message', data => resolve(JSON.parse(data.toString())))
      }))
      peer = await Process.connect(`ws://127.0.0.1:${server.address().port}/rutis`, () => undefined, undefined, { local: 'near' })
      peer.callAsync('x', 'y', []).catch(() => {})
      assert.equal((await first).op, 'hello', `round ${round}`)
    } finally {
      // The far end goes; the session ends with it.
      for (const client of server.clients) client.terminate()
      await peer?.dispose().catch(() => {})
      await new Promise(resolve => server.close(resolve))
    }
  }
})
