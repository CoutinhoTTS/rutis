import { ConnectError } from './errors.mjs'
import * as unix from './unix.mjs'

export { ConnectError }

// open(spec, { message(text), closed(reason) }) → { send(text), end(), close(reason) }
// Specs: `unix:<path>`, or a bare path. Failing to connect throws a
// ConnectError; a later end is reported through `closed`.
export function open(spec, handlers) {
  const scheme = /^([a-z][a-z0-9+.-]*):/.exec(spec)?.[1]
  if (scheme === undefined || scheme === 'unix') return unix.open(spec, handlers)
  return Promise.reject(new ConnectError('incompatible', `no channel for ${scheme}: addresses`))
}
