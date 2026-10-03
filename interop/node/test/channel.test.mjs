import { test } from 'node:test'
import assert from 'node:assert/strict'
import { duplexPair } from 'node:stream'
import { lineChannel } from '../src/channel/lines.mjs'
import { decode, encode } from '../src/codec.mjs'

test('encoded frames carry no framing', () => {
  const text = encode({ op: 'hello', version: 1, note: 'a\nb', missing: undefined })
  assert.ok(!text.includes('\n'))
  assert.deepEqual(decode(text), { op: 'hello', version: 1, note: 'a\nb', missing: null })
  assert.throws(() => encode({ value: Number.NaN }), TypeError)
})

test('a line channel keeps messages whole and in order', async () => {
  const [left, right] = duplexPair()
  const failures = []
  const one = lineChannel(left, { failed: error => failures.push(error) })
  const two = lineChannel(right, { failed: error => failures.push(error) })
  const sent = ['first', encode({ op: 'call', args: ['x\ny'] }), '', 'last']
  for (const message of sent) one.send(message)
  one.end()
  const received = []
  for await (const message of two.messages) received.push(message)
  assert.deepEqual(received, sent)
  assert.deepEqual(failures, [])
})
