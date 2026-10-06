// A Cordis node importing `clock` from a rutis node that imports it from a
// third one, then calling it synchronously with a callback that the far
// owner calls back, and that calls the clock again: the chain must come
// back to this thread, which waits synchronously.
import { Context } from '@deepseek-ai/cordis'
import { Link, Import } from '../../src/bridge/index.mjs'

const [address] = process.argv.slice(2)
const ctx = new Context()
ctx.plugin(Link, { peer: 'a', id: 'c', dial: address, token: 'c-token', retry: { initial: 50, max: 500 } })
ctx.plugin(Import, { peer: 'a', services: ['clock'] })
ctx.plugin({
  name: 'caller',
  inject: ['clock'],
  apply(scope) {
    const clock = scope.clock
    const first = clock.now()
    const days = clock.each(day => `${day.toUpperCase()}@${clock.now()}`)
    process.stdout.write(`${JSON.stringify({ first, days })}\n`)
    clock.later().then(value => { process.stdout.write(`later: ${value}\n`); process.exit(0) })
  },
})
