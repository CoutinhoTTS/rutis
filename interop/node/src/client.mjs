import { Worker, MessageChannel, receiveMessageOnPort } from 'node:worker_threads'
import { Peer } from './peer.mjs'

export class Process {
  #port
  #signal = new Int32Array(new SharedArrayBuffer(4))
  #worker
  #exited
  #peer

  constructor(executable, { socketPath, dispatch } = {}) {
    const { port1, port2 } = new MessageChannel()
    this.#port = port1
    this.#peer = new Peer({
      send: frame => this.#port.postMessage({ frame }),
      abort: () => this.#port.postMessage({ abort: true }),
      dispatch: dispatch ?? (() => { throw new Error('application has no exported service target') }),
      pump: done => {
        const sequence = Atomics.load(this.#signal, 0)
        let packet
        while ((packet = receiveMessageOnPort(this.#port))) this.#receive(packet.message)
        if (!done()) Atomics.wait(this.#signal, 0, sequence)
      },
    })
    this.#port.on('message', message => this.#receive(message))
    this.#worker = new Worker(new URL('./io-worker.mjs', import.meta.url), {
      workerData: { executable, socketPath, port: port2, signal: this.#signal }, transferList: [port2],
    })
    this.#worker.on('error', error => this.#peer.close(error))
    this.#exited = new Promise(resolve => this.#worker.once('exit', code => {
      this.#peer.close(new Error(`communication worker exited (${code})`))
      this.#port.close()
      resolve(code)
    }))
  }
  #receive(message) {
    if (message?.ready) { this.pid = message.pid; this.#peer.start(); return }
    if (message?.closed) { this.#peer.close(new Error(message.closed)); return }
    this.#peer.receive(message)
  }
  static async launch(executable, config) {
    const process = new Process(executable)
    try {
      await process.#peer.ready
      await process.callAsync('', 'mount', { config })
      return process
    } catch (error) {
      process.#port.postMessage({ abort: true })
      await process.#exited
      throw error
    }
  }
  static async connect(socketPath, dispatch) {
    const process = new Process(undefined, { socketPath, dispatch })
    try { await process.#peer.ready; return process }
    catch (error) { process.#port.postMessage({ abort: true }); await process.#exited; throw error }
  }
  call(target, method, args) { return this.#peer.invoke(target, method, args) }
  callAsync(target, method, args) { return this.#peer.invokeAsync(target, method, args) }
  release(value) { this.#peer.release(value) }
  drain() { return this.#peer.drain() }
  closed() { return this.#exited }
  async dispose() {
    this.#port.postMessage({ dispose: true })
    try { await this.callAsync('', 'dispose', null) }
    finally {
      this.#peer.close(new Error('plugin has been disposed'))
      this.#port.postMessage({ end: true })
      await this.#exited
    }
  }
}
