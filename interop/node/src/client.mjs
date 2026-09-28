import { Worker, MessageChannel, receiveMessageOnPort } from 'node:worker_threads'
import { encode } from './wire.mjs'

// The worker owns I/O only. Values are returned on the calling JS thread.
export class Process {
  #port
  #signal = new Int32Array(new SharedArrayBuffer(4))
  #pending = new Map()
  #next = 0
  #closed
  #worker
  #exited
  #ready
  #resolveReady
  #rejectReady

  constructor(executable) {
    this.#ready = new Promise((resolve, reject) => { this.#resolveReady = resolve; this.#rejectReady = reject })
    const { port1, port2 } = new MessageChannel()
    this.#port = port1
    this.#port.on('message', message => this.#receive(message))
    this.#worker = new Worker(new URL('./io-worker.mjs', import.meta.url), {
      workerData: { executable, port: port2, signal: this.#signal }, transferList: [port2],
    })
    this.#worker.on('error', error => this.#close(error))
    this.#exited = new Promise(resolve => this.#worker.once('exit', code => {
      this.#close(new Error(`Rust communication worker exited (${code})`))
      this.#port.close()
      resolve(code)
    }))
  }

  static async launch(executable, config) {
    const process = new Process(executable)
    try {
      await process.#ready
      await process.callAsync('', 'mount', { config })
      return process
    } catch (error) {
      process.#port.postMessage({ abort: true })
      await process.#exited
      throw error
    }
  }

  #close(error) {
    if (this.#closed) return
    this.#closed = error
    this.#rejectReady(error)
    for (const pending of this.#pending.values()) pending.finish(undefined, error)
    this.#pending.clear()
  }

  #receive(message) {
    if (message?.ready) { this.pid = message.pid; this.#resolveReady(); return }
    if (message?.closed) { this.#close(new Error(message.closed)); return }
    const pending = this.#pending.get(message?.id)
    if (!pending || !['ok', 'error'].includes(message.status)) {
      this.#close(new Error('invalid response from Rust process'))
      this.#port.postMessage({ abort: true })
      return
    }
    this.#pending.delete(message.id)
    const error = message.status === 'error' ? Object.assign(new Error(message.message), { name: message.name }) : undefined
    pending.finish(message.value, error)
  }

  #send(target, method, args, finish) {
    if (this.#closed) throw this.#closed
    const id = ++this.#next
    if (!Number.isSafeInteger(id)) throw new Error('call identifiers exhausted')
    const frame = encode({ id, target, method, args })
    this.#pending.set(id, { finish })
    this.#port.postMessage({ frame, dispose: target === '' && method === 'dispose' })
  }

  call(target, method, args) {
    let done = false, value, error
    this.#send(target, method, args, (result, failure) => { done = true; value = result; error = failure })
    while (!done) {
      const sequence = Atomics.load(this.#signal, 0)
      let packet
      while ((packet = receiveMessageOnPort(this.#port))) this.#receive(packet.message)
      if (!done) Atomics.wait(this.#signal, 0, sequence)
    }
    if (error) throw error
    return value
  }

  callAsync(target, method, args) {
    return new Promise((resolve, reject) => this.#send(target, method, args, (value, error) => error ? reject(error) : resolve(value)))
  }

  async dispose() {
    try { await this.callAsync('', 'dispose', null) }
    finally {
      this.#close(new Error('plugin has been disposed'))
      await this.#exited
    }
  }
}
