import { test } from 'node:test'
import assert from 'node:assert/strict'
import { Peer } from '../src/peer.mjs'
const data = value => ({ type: 'data', value })
function harness(dispatch) {
  const sent = [], incoming = []
  let fault
  const peer = new Peer({
    dispatch,
    send: line => sent.push(JSON.parse(line)),
    pump: () => { assert.ok(incoming.length, 'sync caller must not strand'); peer.receive(incoming.shift()) },
    abort: error => { fault = error },
  })
  peer.receive({ op: 'hello', version: 1 })
  return { peer, sent, incoming, fault: () => fault }
}
const invoke = (id, method, path = []) => ({ op: 'invoke', id: `rust:${id}`, path, target: 'test', method, args: data([]) })

test('admitted callback survives a following counted release before dispatch', async () => {
  let calls = 0
  const callback = () => ++calls
  const { peer, sent, fault } = harness(() => [callback, callback])
  peer.receive(invoke(1, 'get'))
  await Promise.resolve()
  const [a, b] = sent[0].value.value
  assert.equal(a.value.id, b.value.id)
  peer.receive({ op: 'call', id: 'rust:2', path: [], reference: a.value.id, args: data([]) })
  peer.receive({ op: 'release', reference: a.value.id, count: 2 })
  assert.equal(calls, 0)
  await Promise.resolve()
  assert.equal(calls, 1)
  assert.equal(sent[1].value.value, 1)
  assert.equal(fault(), undefined)
  peer.close()
})

test('old release preserves a new grant and identifiers are not reused', async () => {
  const callback = () => 42
  const { peer, sent, fault } = harness(() => callback)
  peer.receive(invoke(1, 'get')); await Promise.resolve()
  peer.receive(invoke(2, 'get')); await Promise.resolve()
  const id = sent[0].value.value.id
  assert.equal(sent[1].value.value.id, id)
  peer.receive({ op: 'release', reference: id, count: 1 })
  peer.receive({ op: 'call', id: 'rust:3', path: [], reference: id, args: data([]) })
  await Promise.resolve()
  assert.equal(sent[2].value.value, 42)
  peer.receive({ op: 'release', reference: id, count: 1 })
  peer.receive(invoke(4, 'get')); await Promise.resolve()
  assert.notEqual(sent[3].value.value.id, id)
  assert.equal(fault(), undefined)
  peer.close()
})

test('sync wait only pumps related calls and admits a prequeued related callback once', async () => {
  const seen = []
  const { peer, sent, incoming, fault } = harness((_, method) => { seen.push(method); return 42 })
  peer.receive(invoke(1, 'queued', ['node:1']))
  incoming.push(invoke(2, 'unrelated'), invoke(3, 'related', ['node:1']), { op: 'return', id: 'node:1', value: data(7) })
  assert.equal(peer.invoke('test', 'call', []), 7)
  assert.deepEqual(seen, ['queued', 'related'])
  await Promise.resolve()
  assert.deepEqual(seen, ['queued', 'related', 'unrelated'])
  assert.equal(sent.filter(frame => frame.id === 'rust:1').length, 1)
  assert.equal(fault(), undefined)
  peer.close()
})

test('explicit release accounts repeated live imports without relying on GC', async () => {
  const { peer, sent, incoming } = harness(() => {})
  const ref = { type: 'reference', value: { id: 1, kind: 'function', home: false, origin: [] } }
  incoming.push({ op: 'return', id: 'node:1', value: { type: 'list', value: [ref, ref] } })
  const [a, b] = peer.invoke('test', 'get', [])
  assert.equal(a, b)
  peer.release(a)
  assert.deepEqual(sent.at(-1), { op: 'release', reference: 1, count: 2 })
  assert.throws(() => b(), /released/)
  peer.close()
})

test('unsupported nested references fail explicitly and roll back partial grants', async () => {
  const callback = () => 42
  const { peer, sent, incoming, fault } = harness(() => {})
  assert.throws(() => peer.invoke('test', 'bad', [callback, { nested: callback }]), /unsupported binding/)
  // Failed encoding consumes the invocation id, but publishes no reference.
  incoming.push({ op: 'return', id: 'node:2', value: data(7) })
  assert.equal(peer.invoke('test', 'good', []), 7)
  assert.equal(sent.length, 1)
  assert.equal(fault(), undefined)
  peer.close()
})
