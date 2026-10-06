import { Worker, MessageChannel, receiveMessageOnPort } from 'node:worker_threads'
import { Session } from './session.mjs'

export class Process {
  #port
  #signal = new Int32Array(new SharedArrayBuffer(4))
  #worker
  #exited
  #session

  constructor(executable, { channel, dispatch, settled, endpoint } = {}) {
    const { port1, port2 } = new MessageChannel()
    this.#port = port1
    this.#session = new Session({
      send: frame => this.#port.postMessage({ frame }),
      abort: () => this.#port.postMessage({ abort: true }),
      dispatch: dispatch ?? (() => { throw new Error('application has no exported service target') }),
      settled,
      endpoint,
      pump: done => {
        const sequence = Atomics.load(this.#signal, 0)
        let packet
        while ((packet = receiveMessageOnPort(this.#port))) this.#receive(packet.message)
        if (!done()) Atomics.wait(this.#signal, 0, sequence)
      },
    })
    this.#port.on('message', message => this.#receive(message))
    this.#worker = new Worker(new URL('./io-worker.mjs', import.meta.url), {
      workerData: { executable, channel, port: port2, signal: this.#signal }, transferList: [port2],
    })
    this.#worker.on('error', error => this.#session.close(error))
    this.#exited = new Promise(resolve => this.#worker.once('exit', code => {
      this.#session.close(new Error(`communication worker exited (${code})`))
      this.#port.close()
      resolve(code)
    }))
  }
  #receive(message) {
    if (message?.ready) { this.pid = message.pid; this.#session.start(); return }
    if (message?.closed) { this.#session.close(new Error(message.closed)); return }
    this.#session.receive(message)
  }
  static async launch(executable, config) {
    const process = new Process(executable)
    try {
      await process.#session.ready
      await process.callAsync('', 'mount', { config })
      return process
    } catch (error) {
      process.#port.postMessage({ abort: true })
      await process.#exited
      throw error
    }
  }
  // `endpoint` ({ local, expected? }) selects the endpoint format.
  static async connect(channel, dispatch, settled, endpoint) {
    const process = new Process(undefined, { channel, dispatch, settled, endpoint })
    try { await process.#session.ready; return process }
    catch (error) { process.#port.postMessage({ abort: true }); await process.#exited; throw error }
  }
  get greeting() { return this.#session.greeting }
  call(target, method, args) { return this.#session.invoke(target, method, args) }
  callAsync(target, method, args) { return this.#session.invokeAsync(target, method, args) }
  release(value) { this.#session.release(value) }
  drain() { return this.#session.drain() }
  closed() { return this.#exited }
  async dispose() {
    this.#port.postMessage({ dispose: true })
    try { await this.callAsync('', 'dispose', null) }
    finally {
      this.#session.close(new Error('plugin has been disposed'))
      this.#port.postMessage({ end: true })
      await this.#exited
    }
  }
}
