import { Worker, MessageChannel, receiveMessageOnPort } from 'node:worker_threads'
import { Session } from '../session.mjs'

// The sessions of one link, over its I/O worker. `events` receives
// accepted(generation), closed(generation, reason), failed(error),
// listening(url) and fatal(message); the link decides which connection is
// the session and when it starts (`activate`).
export class Connector {
  #port
  #signal = new Int32Array(new SharedArrayBuffer(4))
  #worker
  #sessions = new Map()
  #events

  constructor({ spec, mode, options, events }) {
    this.#events = events
    const { port1, port2 } = new MessageChannel()
    this.#port = port1
    this.#port.on('message', message => this.#receive(message))
    this.#worker = new Worker(new URL('./worker.mjs', import.meta.url), {
      workerData: { spec, mode, options, port: port2, signal: this.#signal }, transferList: [port2],
    })
    this.#worker.on('error', error => events.fatal?.(error.message))
  }

  #pump(done) {
    const sequence = Atomics.load(this.#signal, 0)
    let packet
    while ((packet = receiveMessageOnPort(this.#port))) this.#receive(packet.message)
    if (!done()) Atomics.wait(this.#signal, 0, sequence)
  }

  #receive(message) {
    if (message.frame) { this.#sessions.get(message.generation)?.receive(message.frame); return }
    if (message.accepted) { this.#events.accepted?.(message.generation); return }
    if (message.closed) {
      const session = this.#sessions.get(message.generation)
      this.#sessions.delete(message.generation)
      session?.close(new Error(message.closed))
      this.#events.closed?.(message.generation, message.closed)
      return
    }
    if (message.failed) {
      const error = new Error(message.failed)
      error.category = message.category
      this.#events.failed?.(error)
      return
    }
    if (message.listening) { this.#events.listening?.(message.listening); return }
    if (message.fatal) this.#events.fatal?.(message.fatal)
  }

  // Start the session of `generation`: its buffered frames flow, and this
  // side greets.
  activate(generation, { dispatch, endpoint }) {
    const session = new Session({
      send: frame => this.#port.postMessage({ generation, frame }),
      abort: () => this.close(generation, 'session failed'),
      dispatch,
      endpoint,
      pump: done => this.#pump(done),
    })
    this.#sessions.set(generation, session)
    this.#port.postMessage({ generation, activate: true })
    session.start()
    return session
  }

  // End `generation`, telling the far end it was replaced (4002).
  replace(generation) {
    this.#sessions.get(generation)?.close(new Error('replaced by a new connection'))
    this.#sessions.delete(generation)
    this.#port.postMessage({ generation, replaced: true })
  }

  close(generation, reason) {
    this.#sessions.get(generation)?.close(new Error(reason))
    this.#sessions.delete(generation)
    this.#port.postMessage({ generation, close: reason })
  }

  async stop() {
    for (const session of this.#sessions.values()) session.close(new Error('link stopped'))
    this.#sessions.clear()
    this.#port.postMessage({ stop: true })
    await this.#worker.terminate()
  }
}
