// A Cordis node meeting the node conformance contract
// (rutis_bridge::conformance): it dials `main` at the given address,
// exports `calendar`, imports `probe`, hosts the npm plugins under `anchor`
// (conformance-greeter among them) and answers `tick` with `tock`.
import { Context } from '@deepseek-ai/cordis'
import { Link, Export, Import, Host, Events } from '../../src/bridge/index.mjs'

const [address, anchor] = process.argv.slice(2)
const ctx = new Context()
ctx.provide('calendar', { today() { return 'monday' }, async later() { return 'tuesday' } })
ctx.plugin(Link, { peer: 'main', id: 'cordis', dial: address, token: 'cordis-token', require: ['node'], retry: { initial: 50, max: 500 } })
ctx.plugin(Export, { peer: 'main', services: { calendar: { today: 'sync', later: 'async' } } })
ctx.plugin(Import, { peer: 'main', services: ['probe'] })
ctx.plugin(Host, { peer: 'main', anchor })
ctx.plugin(Events, { peer: 'main', out: ['tock'], in: ['tick'] })
ctx.on('tick', async (...args) => { await ctx.parallel('tock', 'pong', args.length) })
