// Test-only scalar callback transport. It verifies the synchronous message
// pump mechanism; production Process does not support callback frames yet.
import assert from 'node:assert/strict'
import { Worker, MessageChannel, receiveMessageOnPort, isMainThread, threadId, workerData } from 'node:worker_threads'
import { createConnection } from 'node:net'
import { createInterface } from 'node:readline'

if (!isMainThread) {
  const { port, signal, socket } = workerData
  const connection = createConnection(socket)
  const send = message => {
    port.postMessage(message)
    Atomics.add(signal, 0, 1)
    Atomics.notify(signal, 0)
  }
  const lines = createInterface({ input: connection })
  port.on('message', message => connection.write(JSON.stringify(message) + '\n'))
  lines.on('line', line => send(JSON.parse(line)))
  connection.on('error', error => send({ type: 'error', message: error.message }))
  connection.on('close', () => send({ type: 'error', message: 'closed' }))
} else {
  const { port1: port, port2 } = new MessageChannel()
  const signal = new Int32Array(new SharedArrayBuffer(4))
  const worker = new Worker(new URL(import.meta.url), {
    workerData: { port: port2, signal, socket: process.argv[2] }, transferList: [port2],
  })
  let timerRan = false, continuationRan = false, callbacks = 0
  const timer = new Promise(resolve => setTimeout(() => { timerRan = true; resolve() }, 0))
  const continuation = Promise.resolve().then(() => { continuationRan = true })
  const originalThread = threadId
  // This closure never leaves its owning JS thread.
  const callback = value => {
    assert.equal(threadId, originalThread)
    assert.equal(timerRan, false)
    assert.equal(continuationRan, false)
    callbacks++
    return value + 1
  }
  try {
    port.postMessage({ type: 'invoke' })
    let result
    const deadline = Date.now() + 5000
    while (result === undefined) {
      const sequence = Atomics.load(signal, 0)
      let packet
      while ((packet = receiveMessageOnPort(port))) {
        const message = packet.message
        if (message.type === 'callback') {
          port.postMessage({ type: 'callback-result', value: callback(message.value) })
        } else if (message.type === 'result') result = message.value
        else throw new Error(message.message ?? 'unexpected frame')
      }
      const remaining = deadline - Date.now()
      assert.ok(remaining > 0, 'callback probe timed out')
      if (result === undefined) Atomics.wait(signal, 0, sequence, remaining)
    }
    assert.equal(result, 42)
    assert.equal(callbacks, 1)
    assert.equal(timerRan, false)
    assert.equal(continuationRan, false)
    await Promise.all([timer, continuation])
    assert.equal(timerRan, true)
    assert.equal(continuationRan, true)
    process.stdout.write(JSON.stringify({ result, callbacks, resumed: true }) + '\n')
  } finally {
    port.close()
    await worker.terminate()
  }
}
