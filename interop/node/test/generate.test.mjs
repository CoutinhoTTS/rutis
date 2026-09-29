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

test('unsupported members are reported with their source location, not dropped silently', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    class Service { bytes(): Uint8Array { return new Uint8Array() } ping(): number { return 1 } }
    export function apply(ctx: Context) { ctx.provide('session', new Service()) }
  `, file => {
    const { rust, diagnostics } = generate(file, root)
    assert.equal(diagnostics.length, 1)
    assert.match(diagnostics[0], /plugin\.ts:\d+:\d+: session\.bytes is not bound: Uint8Array/)
    assert.match(rust, /pub fn ping\(&self\)/)
    assert.match(rust, /Members not bound yet:\n\/\/\/ - `bytes`/)
    assert.doesNotMatch(rust, /fn bytes\(/)
  })
})

test('objects with methods become live object proxies with getters and methods', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    export interface Session { readonly id: string; readonly turns: number; close(): Promise<void>; rename(title: string): Session }
    class Service { open(title: string): Session { throw new Error() } list(): Session[] { return [] } }
    export function apply(ctx: Context) { ctx.provide('sessions', new Service()) }
  `, file => {
    const { rust, diagnostics } = generate(file, root)
    assert.deepEqual(diagnostics, [])
    assert.match(rust, /pub struct Session\(pub ::rutis_interop::ObjectRef\);/)
    assert.match(rust, /pub fn id\(&self\) -> Result<String, ::rutis_interop::Error> \{\s*::rutis_interop::decode_value\(self\.0\.get\("id"\)\?\)/)
    assert.match(rust, /pub async fn close\(&self\) -> Result<\(\), ::rutis_interop::Error>/)
    assert.match(rust, /pub fn rename\(&self, title: &str\) -> Result<Session, ::rutis_interop::Error>/)
    assert.match(rust, /pub fn open\(&self, title: &str\) -> Result<Session, ::rutis_interop::Error>/)
    assert.match(rust, /pub fn list\(&self\) -> Result<Vec<Session>, ::rutis_interop::Error>/)
  })
})

test('public state cannot silently become a copied value', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    class Service { value = 1; read(): number { return this.value } }
    export function apply(ctx: Context) { ctx.provide('state', new Service()) }
  `, file => {
    // Bound as a live read of the service object, never a copied field.
    const { rust, diagnostics } = generate(file, root)
    assert.deepEqual(diagnostics, [])
    assert.doesNotMatch(rust, /pub value/)
    assert.match(rust, /pub fn value\(&self\) -> Result<f64, ::rutis_interop::Error> \{\s*::rutis_interop::decode_value\(self\.process\.get\(&self\.handle, "value"\)\?\)/)
  })
})

test('a Service class provides the Context members typed as it or its bases', async () => {
  await fixture(`import { Context, Service } from '@deepseek-ai/cordis'
    declare module '@deepseek-ai/cordis' { interface Context { store: Store } }
    export type Key = string & { readonly __brand: 'Key' }
    export type Mode = 'read' | 'write'
    export interface Entry { key: Key; mode: Mode; size?: number; tags: readonly string[]; meta: Record<string, number> | null }
    export abstract class Store extends Service {
      constructor(ctx: Context) { super(ctx, 'store') }
      abstract get(key: Key, signal?: AbortSignal): Promise<Entry | undefined>
      abstract put(entry: Entry, overwrite?: boolean): void
    }
    export interface Config { root: string; limit?: number }
    export default class MemoryStore extends Store {
      constructor(ctx: Context, config: Config) { super(ctx) }
      async get(key: Key) { return undefined }
      put(entry: Entry, overwrite?: boolean) {}
    }
  `, file => {
    const { rust, diagnostics } = generate(file, root)
    assert.deepEqual(diagnostics, [])
    assert.match(rust, /pub struct Key\(pub String\)/)
    assert.match(rust, /pub enum Mode \{ #\[serde\(rename = "read"\)\] Read, #\[serde\(rename = "write"\)\] Write, \}/)
    assert.match(rust, /pub size: Option<f64>/)
    assert.match(rust, /pub tags: Vec<String>/)
    assert.match(rust, /pub meta: Option<::std::collections::BTreeMap<String, f64>>/)
    assert.match(rust, /pub async fn get\(&self, key: &Key\) -> Result<Option<Entry>, ::rutis_interop::Error>/)
    // The AbortSignal is not a Rust parameter: dropping the future aborts it.
    assert.match(rust, /Cancellable: dropping the returned future/)
    assert.match(rust, /vec!\[::rutis_interop::arg\(&key\)\?, ::rutis_interop::rpc::Value::Signal\]/)
    assert.match(rust, /pub fn put\(&self, entry: &Entry, overwrite: Option<bool>\)/)
    assert.match(rust, /::rutis_interop::optional\(overwrite\)\?/)
    assert.match(rust, /pub struct Config \{ #\[serde\(rename = "root"\)\] pub root: String,/)
    assert.match(rust, /projection\.service::<Store>\("store"/)
  })
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

test('a group gets one Config field per member and rejects a service provided twice', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    export interface Config { now: number }
    export function apply(ctx: Context, config: Config) { ctx.provide('clock', { now(): number { return config.now } }) }
  `, async (clock, temporary) => {
    const greeter = join(temporary, 'greeter.ts')
    await writeFile(greeter, `import type { Context } from '@deepseek-ai/cordis'
      export const inject = ['clock']
      export function apply(ctx: Context) { ctx.provide('greeter', { greet(name: string): string { return name } }) }
    `)
    const { rust } = generate([{ name: 'clock', path: clock }, { name: 'greeter', path: greeter }], root)
    assert.match(rust, /pub struct Config \{ pub clock: ClockConfig, pub greeter: GreeterConfig, \}/)
    assert.match(rust, /pub struct ClockConfig \{ #\[serde\(rename = "now"\)\] pub now: f64,/)
    assert.match(rust, /Process::launch_mount\(/)
    assert.match(rust, /self\.config\.clock\.now\.is_finite\(\)/)
    assert.throws(() => generate([{ name: 'a', path: clock }, { name: 'b', path: clock }], root), /service clock is provided by both/)
  })
})

test('a host-provided service becomes a trait, a dispatcher and an injected dependency', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    declare module '@deepseek-ai/cordis' { interface Context { prompt: Prompt } }
    export type Slot = 'prefix' | 'suffix'
    export interface Section { name: string; order: number; text: string | ((scope: string) => string) }
    export interface Prompt {
      section(section: Section): () => void
      order(slot: Slot): number
      render(signal?: AbortSignal): Promise<string>
      provider(provide: (scope: string) => string): () => void
    }
    export const inject = ['prompt']
    export function apply(ctx: Context) { ctx.effect(() => ctx.prompt.section({ name: 'a', order: ctx.prompt.order('prefix'), text: 'hi' })) }
  `, file => {
    const { rust, diagnostics } = generate(file, root, { provide: ['prompt'] })
    assert.match(diagnostics.join('\n'), /host prompt\.section: function values in .* are not bound; only its data members are/)
    assert.match(rust, /pub trait PromptHost: Send \+ Sync \+ 'static/)
    assert.match(rust, /fn section\(&self, section: Section\) -> Result<::rutis_interop::rpc::Value, ::rutis_interop::Error>/)
    assert.match(rust, /fn order\(&self, slot: Slot\) -> Result<f64, ::rutis_interop::Error>/)
    assert.match(rust, /fn render\(&self\) -> ::rutis::BoxFuture<'static, Result<String, ::rutis_interop::Error>>/)
    assert.match(rust, /fn provider\(&self, provide: ::rutis_interop::rpc::Value\)/)
    assert.match(rust, /pub text: String,/)
    assert.match(rust, /pub fn provide_prompt\(ctx: &::rutis::Ctx, host: impl PromptHost\)/)
    assert.match(rust, /injects: vec!\[::rutis::TypeKey::of::<dyn PromptHost>\(\)\]/)
    assert.match(rust, /"section":"sync","order":"sync","render":"async","provider":"sync"/)
    assert.throws(() => generate(file, root, { provide: ['missing'] }), /no Cordis Context declaration found for the host-provided service missing/)
  })
})

test('callback parameters take Rust closures and returned functions stay remote', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    class Service {
      watch(path: string, changed: (error?: Error) => void): () => Promise<void> { return async () => {} }
      update(mutate: (current: number | undefined) => Promise<number>): Promise<number> { return mutate(undefined) }
      install(installer: (ctx: Context) => void | Promise<void>): () => void { return () => {} }
      maybe(hook?: () => void): void {}
    }
    export function apply(ctx: Context) { ctx.provide('hooks', new Service()) }
  `, file => {
    const { rust, diagnostics } = generate(file, root)
    assert.match(rust, /pub fn watch\(&self, path: &str, changed: impl Fn\(Option<::rutis_interop::JsError>\) -> Result<\(\), ::rutis_interop::Error> \+ Send \+ Sync \+ 'static\) -> Result<::rutis_interop::RemoteFunction, ::rutis_interop::Error>/)
    assert.match(rust, /pub async fn update\(&self, mutate: impl Fn\(Option<f64>\) -> ::rutis::BoxFuture<'static, Result<f64, ::rutis_interop::Error>> \+ Send \+ Sync \+ 'static\)/)
    assert.match(rust, /install\(&self, installer: impl Fn\(Context\) -> ::rutis::BoxFuture<'static, Result<\(\), ::rutis_interop::Error>>/)
    assert.match(rust, /let changed = ::rutis_interop::rpc::Value::callback\(move \|args\|/)
    assert.match(diagnostics.join('\n'), /hooks\.maybe is not bound: optional callback parameter/)
  })
})
