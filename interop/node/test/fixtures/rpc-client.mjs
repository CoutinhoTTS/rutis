import assert from 'node:assert/strict'
import { threadId } from 'node:worker_threads'
import { Process } from '../../src/client.mjs'

const peer = await Process.connect(process.argv[2], () => { throw new Error('no root exports') })
const owner = threadId
let timerRan = false, promiseRan = false
const timer = setTimeout(() => { timerRan = true }, 0)
Promise.resolve().then(() => { promiseRan = true })
assert.equal(peer.call('rpc', 'apply', [n => {
  assert.equal(threadId, owner)
  assert.equal(timerRan, false)
  assert.equal(promiseRan, false)
  return peer.call('rpc', 'add', [n, 1])
}, 4]), 5)
const saved = n => n * 2
peer.call('rpc', 'save', [saved])
assert.equal(peer.call('rpc', 'fire', [7]), 14)
assert.equal(peer.call('rpc', 'saved', []), saved)
peer.call('rpc', 'clear', [])
assert.equal(peer.call('rpc', 'onlyInvoke', [() => Promise.resolve(42)]), true)
assert.equal(await peer.callAsync('rpc', 'awaitCallback', [async () => {
  await new Promise(resolve => setTimeout(resolve, 2))
  return 42
}]), 42)
assert.throws(() => peer.call('rpc', 'syncAwait', [() => new Promise(resolve => setTimeout(() => resolve(42), 2))]), { name: 'SyncWaitCycle' })
const cause = new Error('cause')
const error = new AggregateError([new TypeError('first', { cause })], 'outer', { cause })
try { peer.call('rpc', 'apply', [() => { throw error }, 0]); assert.fail('must throw') }
catch (crossed) {
  assert.ok(crossed instanceof AggregateError)
  assert.ok(crossed.errors[0] instanceof TypeError)
  assert.equal(crossed.cause, crossed.errors[0].cause)
  assert.equal(crossed.stack, error.stack)
}
// Closing is explicit even while a remote-owned callback remains reachable.
peer.call('rpc', 'save', [saved])
clearTimeout(timer)
await peer.dispose()
