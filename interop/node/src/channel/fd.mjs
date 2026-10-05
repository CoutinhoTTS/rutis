import { Socket } from 'node:net'
import { ConnectError } from './errors.mjs'
import { frame } from './unix.mjs'

// An inherited socket: `fd:<n>`, given by the process that started this one.
export async function open(spec, handlers) {
  const fd = Number(spec.slice('fd:'.length))
  if (!Number.isSafeInteger(fd) || fd < 0) throw new ConnectError('incompatible', `invalid channel ${spec}`)
  let stream
  try {
    stream = new Socket({ fd, readable: true, writable: true })
  } catch (error) {
    throw new ConnectError('incompatible', `${spec}: ${error.message}`)
  }
  return frame(stream, handlers)
}
