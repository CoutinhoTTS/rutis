// A Cordis application linked to a rutis node as a full node: it exports
// `calendar`, imports `clock`, hosts the npm plugins under `anchor`, and
// forwards events (`tick` in, `tock` out). What happens is printed on
// stdout, one line each, for the test driving it.
import { Context } from '@deepseek-ai/cordis'
import { Link, Export, Import, Host, Events } from '../../src/bridge/index.mjs'

const [address, anchor] = process.argv.slice(2)
const say = line => process.stdout.write(`${line}\n`)
const ctx = new Context()
ctx.provide('calendar', { today() { return 'monday' }, async later() { return 'tuesday' } })
// `listen:<address>` listens for main instead of dialing it.
const where = address.startsWith('listen:') ? { listen: address.slice('listen:'.length) } : { dial: address }
ctx.plugin(Link, { peer: 'main', id: 'mac', ...where, token: 'mac-token', require: ['node'], retry: { initial: 50, max: 500 } })
ctx.plugin(Export, { peer: 'main', services: { calendar: { today: 'sync', later: 'async' } } })
ctx.plugin(Import, { peer: 'main', services: ['clock'] })
ctx.plugin(Host, { peer: 'main', anchor })
ctx.plugin(Events, { peer: 'main', out: ['tock'], in: ['tick'] })
ctx.plugin({ name: 'clock-user', inject: ['clock'], apply(scope) { say(`clock: ${scope.clock.now()}`) } })
ctx.on('tick', async (...args) => {
  say(`tick: ${JSON.stringify(args)}`)
  await ctx.parallel('tock', 'pong', args.length)
})
say('started')
// The link's state changes, on stderr, for diagnosing a failing test.
let shown
setInterval(() => {
  const status = ctx.get('rutisLink.main', false)
  const text = JSON.stringify(status)
  if (text !== shown) { shown = text; process.stderr.write(`link: ${text}\n`) }
}, 50).unref()
