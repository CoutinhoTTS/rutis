import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtemp, writeFile, rm } from 'node:fs/promises'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { generate } from '../src/generate.mjs'

const directory = dirname(fileURLToPath(import.meta.url))
const root = resolve(directory, '..')

async function fixture(body, run) {
  const temporary = await mkdtemp(join(directory, 'generator-'))
  try {
    const file = join(temporary, 'plugin.ts')
    await writeFile(file, body)
    await run(file, temporary)
  } finally { await rm(temporary, { recursive: true, force: true }) }
}

test('unsupported public objects produce a source diagnostic instead of losing methods', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    class Service { session(): { close(): void } { return { close() {} } } }
    export function apply(ctx: Context) { ctx.provide('session', new Service()) }
  `, file => assert.throws(() => generate(file, root), /plugin\.ts:\d+:\d+: binding not implemented/))
})

test('public state cannot silently become a copied value', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    class Service { value = 1; read(): number { return this.value } }
    export function apply(ctx: Context) { ctx.provide('state', new Service()) }
  `, file => assert.throws(() => generate(file, root), /property binding not implemented for value/))
})

test('ordinary imported interfaces participate in Cargo regeneration', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    import type { Settings } from './settings.js'
    export function apply(ctx: Context, config: Settings) {
      ctx.provide('reader', { read(): number { return config.initial } })
    }
  `, async (file, temporary) => {
    const imported = join(temporary, 'settings.ts')
    await writeFile(imported, 'export interface Settings { initial: number }')
    assert.ok(generate(file, root).inputs.includes(imported))
    await writeFile(imported, 'export interface Settings { initial: { value: number } }')
    assert.throws(() => generate(file, root), /not assignable to type 'number'/)
  })
})
