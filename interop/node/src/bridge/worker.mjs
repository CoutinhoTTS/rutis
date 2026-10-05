import { workerData } from 'node:worker_threads'
import * as websocket from '../channel/websocket.mjs'
import { decode } from '../codec.mjs'

// The I/O of a link, off the main thread so a synchronous call there can
// wait while frames keep arriving. Each connection is a generation: its
// frames are tagged with it, and a listening link's newer connection waits
// (its frames buffered) until the main thread has ended the older session
// and activates it.
const { spec, mode, options, port, signal } = workerData
function send(message) {
  port.postMessage(message)
  Atomics.add(signal, 0, 1)
  Atomics.notify(signal, 0)
}
process.on('uncaughtExceptionMonitor', error => send({ fatal: error.message }))

const connections = new Map()
let next = 0, listener

function handlersFor(generation) {
  return {
    message: text => {
      const connection = connections.get(generation)
      if (!connection) return
      let frame
      try { frame = decode(text) } catch (error) { connection.channel?.close(error.message); return }
      if (connection.active) send({ generation, frame })
      else connection.buffer.push(frame)
    },
    closed: reason => {
      if (!connections.delete(generation)) return
      send({ generation, closed: reason ?? 'the far end disconnected' })
    },
  }
}

port.on('message', message => {
  const connection = connections.get(message.generation)
  if (message.frame) { connection?.channel?.send(message.frame); return }
  if (message.activate && connection) {
    connection.active = true
    for (const frame of connection.buffer.splice(0)) send({ generation: message.generation, frame })
    return
  }
  if (message.replaced && connection) { connections.delete(message.generation); connection.channel?.replaced(); return }
  if (message.close && connection) { connections.delete(message.generation); connection.channel?.close(message.close); return }
  if (message.stop) {
    listener?.close()
    for (const { channel } of connections.values()) channel?.close('link stopped')
    connections.clear()
    port.close()
  }
})

try {
  if (mode === 'dial') {
    // Buffered like a listener's: frames flow once the session exists.
    const generation = ++next
    connections.set(generation, { channel: undefined, active: false, buffer: [] })
    const channel = await websocket.open(spec, handlersFor(generation), options)
    connections.get(generation).channel = channel
    send({ generation, accepted: true })
  } else {
    let latest
    listener = await websocket.listen(spec, {
      handlers: () => {
        latest = ++next
        connections.set(latest, { channel: undefined, active: false, buffer: [] })
        return handlersFor(latest)
      },
      accepted: channel => {
        const generation = latest
        const connection = connections.get(generation)
        if (!connection) return channel.close('link stopped')
        connection.channel = channel
        send({ generation, accepted: true })
      },
    }, options)
    send({ listening: listener.url })
  }
} catch (error) {
  send({ failed: error.message, category: error.category ?? 'retryable' })
}
