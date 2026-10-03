import { createInterface } from 'node:readline'

// A byte stream (a Unix socket, an inherited file descriptor) carrying one
// message per line. Messages must not contain a newline; JSON never does.
// Runs in the I/O worker, which keeps it moving while the main thread is
// blocked in a synchronous call.
export function lineChannel(stream, { failed }) {
  const messages = createInterface({ input: stream })
  stream.on('error', failed)
  messages.on('error', error => { failed(error); stream.destroy() })
  stream.once('close', () => messages.close())
  return {
    messages,
    send: message => stream.write(`${message}\n`),
    end: () => stream.end(),
    destroy: () => stream.destroy(),
  }
}
