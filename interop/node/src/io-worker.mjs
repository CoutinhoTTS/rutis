import { workerData } from 'node:worker_threads'
import { createServer } from 'node:net'
import { spawn } from 'node:child_process'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { open } from './channel/index.mjs'
import { frame } from './channel/unix.mjs'
import { decode } from './codec.mjs'

const { executable, socketPath, port, signal } = workerData
function send(message) {
  port.postMessage(message)
  Atomics.add(signal, 0, 1)
  Atomics.notify(signal, 0)
}

// Main-thread Worker error handlers cannot run during Atomics.wait(). Publish
// an uncaught worker failure before Node terminates the communication thread.
process.on('uncaughtExceptionMonitor', error => send({ closed: error.message }))

let directory, server, child, channel, exited
let failure = 'Rust process disconnected'
let disposing = false
port.on('message', message => {
  if (message.abort) { child?.kill(); channel?.close(); return }
  if (message.dispose) disposing = true
  if (message.end) { channel?.end(); return }
  if (message.frame) channel?.send(message.frame)
})

try {
  let ended
  const done = new Promise(resolve => { ended = resolve })
  const handlers = {
    message: text => {
      try { send(decode(text)) } catch (error) { channel.close(error.message) }
    },
    closed: reason => { if (reason) failure = reason; ended() },
  }
  if (socketPath) {
    channel = await open(socketPath, handlers)
  } else {
    // The frozen reverse direction: listen, then start the Rust process,
    // which dials back.
    directory = await mkdtemp(join(tmpdir(), 'rutis-mount-'))
    const socket = join(directory, 'peer.sock')
    server = createServer()
    const connected = new Promise(resolve => server.once('connection', resolve))
    await new Promise((resolve, reject) => { server.once('error', reject); server.listen(socket, resolve) })
    child = spawn(executable, [socket], { stdio: ['ignore', 'inherit', 'inherit'] })
    exited = new Promise((resolve, reject) => {
      child.once('error', reject)
      child.once('close', (code, signal) => resolve({ code, signal }))
    })
    const stream = await Promise.race([connected, exited.then(status => { throw new Error(`Rust process exited before connecting (${status.code ?? status.signal})`) })])
    server.close()
    channel = frame(stream, handlers)
  }
  send({ ready: true, pid: child?.pid })
  await done
  if (child && !disposing && child.exitCode === null && child.signalCode === null) child.kill()
  const status = await exited
  if (status && status.code !== 0) failure = `Rust process exited (${status.code ?? status.signal})`
} catch (error) {
  failure = error.message
} finally {
  channel?.close()
  server?.close()
  if (child && child.exitCode === null && child.signalCode === null) child.kill()
  await exited?.catch(() => {})
  if (directory) await rm(directory, { recursive: true, force: true })
  send({ closed: failure })
  port.close()
}
