import { spawn } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import * as websocket from './channel/websocket.mjs'
import { frame } from './channel/unix.mjs'
import { ENDPOINT_PROTOCOL } from './session.mjs'

// A runtime that stays up and listens for its controller. Each session runs
// in a child runner of its own, relayed message by message between the
// WebSocket and the child's inherited socket, so a session's lease (its
// rows, proxies, references, its Cordis Context) ends with that child. A
// newer connection takes over: the old one is closed as replaced, the old
// child cleans up and exits, and only then does a new child greet.
export async function serve({ spec, id, peer, anchor }) {
  const runner = fileURLToPath(new URL('./runner.mjs', import.meta.url))
  let current
  let switching = Promise.resolve()
  let latest

  // Messages that arrive before the session's child is up wait for it.
  function relay() {
    const pending = []
    let target, ended = false
    return {
      handlers: {
        message: text => target ? target.send(text) : pending.push(text),
        closed: () => { ended = true; target?.end() },
      },
      attach(channel) {
        target = channel
        for (const text of pending.splice(0)) channel.send(text)
        if (ended) channel.end()
      },
      get ended() { return ended },
    }
  }

  async function stop(session) {
    session.childChannel?.close('replaced by a new connection')
    if (session.child.exitCode === null && session.child.signalCode === null) {
      const exited = new Promise(resolve => session.child.once('exit', resolve))
      const killed = setTimeout(() => session.child.kill('SIGKILL'), 5000)
      await exited
      clearTimeout(killed)
    }
  }

  function start(ws, link) {
    const args = [...process.execArgv, runner, 'fd:3', '--id', id, '--format', 'endpoint']
    if (peer) args.push('--peer', peer)
    args.push(anchor)
    const child = spawn(process.execPath, args, { stdio: ['ignore', 'inherit', 'inherit', 'pipe'] })
    const session = { ws, child }
    session.childChannel = frame(child.stdio[3], {
      message: text => ws.send(text),
      closed: () => { if (current === session) ws.close('runtime session ended') },
    })
    child.once('exit', () => { if (current === session) { current = undefined; ws.close('runtime session ended') } })
    link.attach(session.childChannel)
    return session
  }

  const listener = await websocket.listen(spec, {
    handlers: () => { latest = relay(); return latest.handlers },
    accepted: ws => {
      const link = latest
      switching = switching.then(async () => {
        if (current) {
          const old = current
          current = undefined
          old.ws.replaced()
          await stop(old)
        }
        if (link.ended) return
        current = start(ws, link)
      })
    },
  }, { protocol: `rutis.${ENDPOINT_PROTOCOL}`, ...websocket.optionsFromEnvironment() })
  process.stderr.write(`rutis-interop: listening on ${listener.url}\n`)
  // Serves until killed.
  await new Promise(() => {})
}
