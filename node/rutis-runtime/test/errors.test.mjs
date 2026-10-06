import { test } from 'node:test'
import assert from 'node:assert/strict'
import { encodeError, decodeError } from '../src/errors.mjs'
const roundtrip = value => decodeError(JSON.parse(JSON.stringify(encodeError(value))))

test('thrown primitives, shared objects and cycles survive error transport', () => {
  for (const value of [undefined, null, false, 0, '', 12n, NaN, Infinity, -Infinity]) {
    assert.ok(Object.is(roundtrip(value), value))
  }
  const shared = { value: 4 }
  const input = [shared, shared]
  shared.parent = input
  const output = roundtrip(input)
  assert.equal(output[0], output[1])
  assert.equal(output[0].parent, output)
})

test('custom error names and object keys do not select inherited constructors or setters', () => {
  const error = new Error('message', { cause: JSON.parse('{"__proto__":{"a":1}}') })
  error.name = 'constructor'
  const output = roundtrip(error)
  assert.ok(output instanceof Error)
  assert.equal(output.name, 'constructor')
  assert.equal(Object.getPrototypeOf(output.cause), Object.prototype)
  assert.deepEqual(output.cause.__proto__, { a: 1 })
  assert.throws(() => roundtrip(new Error('unsupported', { cause: () => {} })), /not supported/)
})
