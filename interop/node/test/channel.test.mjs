import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createServer } from 'node:net'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createInterface } from 'node:readline'
import { encode, decode } from '../src/codec.mjs'
import { open, ConnectError } from '../src/channel/index.mjs'

test('the codec adds no separator and escapes newlines inside strings', () => {
  const text = encode({ op: 'hello', note: 'a\nb', missing: undefined })
  assert.equal(text, '{"op":"hello","note":"a\\nb","missing":null}')
  assert.ok(!text.includes('\n'))
  assert.deepEqual(decode(text), { op: 'hello', note: 'a\nb', missing: null })
  assert.throws(() => encode({ n: Infinity }), TypeError)
})

test('a unix channel frames each message as one line, both ways', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'rutis-channel-'))
  const path = join(directory, 'peer.sock')
  const server = createServer()
  const accepted = new Promise(resolve => server.once('connection', resolve))
  await new Promise(resolve => server.listen(path, resolve))
  try {
    const received = []
    let closed
    const ended = new Promise(resolve => { closed = resolve })
    const channel = await open(`unix:${path}`, { message: text => received.push(text), closed })
    const remote = await accepted
    const lines = createInterface({ input: remote })[Symbol.asyncIterator]()
    channel.send(encode({ op: 'hello', note: 'x\ny' }))
    assert.deepEqual(decode((await lines.next()).value), { op: 'hello', note: 'x\ny' })
    remote.write('{"a":1}\n{"b":2}\n')
    remote.end()
    assert.equal(await ended, undefined)
    assert.deepEqual(received, ['{"a":1}', '{"b":2}'])
  } finally {
    server.close()
    await rm(directory, { recursive: true, force: true })
  }
})

test('failing to connect carries a category, not just text', async () => {
  const missing = join(tmpdir(), `rutis-missing-${process.pid}.sock`)
  await assert.rejects(open(missing, { message() {}, closed() {} }),
    error => error instanceof ConnectError && error.category === 'retryable')
  await assert.rejects(open('wss://example.com/rutis', { message() {}, closed() {} }),
    error => error instanceof ConnectError && error.category === 'incompatible')
})
