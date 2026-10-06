// schemastery → JSON Schema, on schema objects shaped like schemastery's.
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { toJsonSchema } from '../src/schema.mjs'

const s = (type, meta = {}, extra = {}) => ({ type, meta, ...extra })

test('object fields, defaults, descriptions, required and volatile', () => {
  const schema = s('object', {}, { dict: {
    name: s('string', { default: 'x', description: { en: 'the name', zh: '名字' } }),
    level: s('number', { step: 1, min: 0, default: 1, volatile: true }),
    on: s('boolean', { required: true }),
  } })
  assert.deepEqual(toJsonSchema(schema), {
    type: 'object',
    properties: {
      name: { type: 'string', default: 'x', description: 'the name' },
      level: { type: 'integer', minimum: 0, default: 1, 'x-volatile': true },
      on: { type: 'boolean' },
    },
    required: ['on'],
  })
})

test('collections, unions and unknown shapes', () => {
  assert.deepEqual(toJsonSchema(s('array', {}, { inner: s('string') })), { type: 'array', items: { type: 'string' } })
  assert.deepEqual(toJsonSchema(s('dict', {}, { inner: s('number') })), { type: 'object', additionalProperties: { type: 'number' } })
  assert.deepEqual(toJsonSchema(s('union', {}, { list: [s('const', {}, { value: 'a' }), s('const', {}, { value: 'b' })] })), { anyOf: [{ const: 'a' }, { const: 'b' }] })
  assert.deepEqual(toJsonSchema(s('transform', {}, { inner: s('natural') })), { type: 'integer', minimum: 0 })
  assert.deepEqual(toJsonSchema(s('something-new')), {})
  assert.deepEqual(toJsonSchema(undefined), {})
})
