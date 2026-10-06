import { createConnection } from 'node:net'
import { createInterface } from 'node:readline'
import { ConnectError } from './errors.mjs'

// How long a channel that ended its side waits for the far end's end.
const END_GRACE = 1000

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
    // Half-close, then wait for the far end's end, but not for ever: on
    // macOS a socket's end may not reach this side after it half-closed.
    end() { stream.end(); setTimeout(() => stream.destroy(), END_GRACE).unref() },
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
