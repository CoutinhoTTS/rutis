// WebSocket failures across implementations.
//   dial <url> [protocol]  prints `connected` or the ConnectError category
//   listen <url> [protocol] prints `rutis-interop: listening on <url>` on stderr,
//                          then serves until killed
// Credentials, CA, certificate and key come from RUTIS_INTEROP_* variables.
import * as websocket from '../../src/channel/websocket.mjs'

const [mode, url, protocol = 'rutis.3'] = process.argv.slice(2)
const options = { protocol, ...websocket.optionsFromEnvironment() }
const handlers = { message() {}, closed() {} }
if (mode === 'dial') {
  try {
    const channel = await websocket.open(url, handlers, options)
    process.stdout.write('connected\n')
    channel.close('done')
  } catch (error) {
    process.stdout.write(`${error.category ?? 'uncategorized'}\n`)
    process.stderr.write(`ws-probe: ${error.message}\n`)
  }
  process.exit(0)
} else {
  const listener = await websocket.listen(url, { handlers: () => handlers, accepted: () => {} }, options)
  process.stderr.write(`rutis-interop: listening on ${listener.url}\n`)
}
