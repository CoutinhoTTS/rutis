import { createConnection } from 'node:net'
import { createInterface } from 'node:readline'
import { ConnectError } from './errors.mjs'

// A newline-framed channel on a connected stream: each message sent gets a
// trailing newline, each line received is one message. `closed(reason)`
// runs once, when the stream ends (reason undefined) or fails.
export function frame(stream, { message, closed }) {
  let failure
  stream.on('error', error => { failure ??= error.message })
  const lines = createInterface({ input: stream })
  lines.on('error', error => { failure ??= error.message; stream.destroy() })
  lines.on('line', line => message(line))
  stream.once('close', () => { lines.close(); closed(failure) })
  return {
    send(text) { stream.write(text + '\n') },
    end() { stream.end() },
    close(reason) { failure ??= reason; stream.destroy() },
  }
}

// Dial a Unix socket: `unix:<path>` or a bare path.
export async function open(spec, handlers) {
  const path = spec.startsWith('unix:') ? spec.slice('unix:'.length) : spec
  const stream = createConnection(path)
  try {
    await new Promise((resolve, reject) => { stream.once('connect', resolve); stream.once('error', reject) })
  } catch (error) {
    stream.destroy()
    throw new ConnectError(error.code === 'EACCES' ? 'auth-rejected' : 'retryable', `${path}: ${error.message}`)
  }
  return frame(stream, handlers)
}
