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

test('a generic live object that keeps wrapping itself stays an untyped reference', async () => {
  // Zod-style schemas: `optional()` wraps the schema in a new instantiation
  // of the same class, so expanding the methods never reaches a known type.
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    export class Schema<T> { optional(): Schema<Schema<T>> { throw new Error() } describe(): string { return '' } }
    class Service { schema(): Schema<string> { throw new Error() } }
    export function apply(ctx: Context) { ctx.provide('schemas', new Service()) }
  `, file => {
    const { rust, diagnostics } = generate(file, root)
    assert.match(rust, /pub fn schema\(&self\) -> Result<Schema, ::rutis_interop::Error>/)
    assert.match(rust, /pub fn optional\(&self\) -> Result<::rutis_interop::ObjectRef, ::rutis_interop::Error>/)
    assert.match(rust, /pub fn describe\(&self\) -> Result<String, ::rutis_interop::Error>/)
    assert.equal(diagnostics.length, 1)
    assert.match(diagnostics[0], /plugin\.ts:\d+:\d+: Schema\.optional: Schema<Schema<string>> wraps Schema<string> again; bound as an untyped ObjectRef/)
  })
})

test('relocatable mounts address the runtime and plugins from the npm project', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    class Service { ping(): number { return 1 } }
    export function apply(ctx: Context) { ctx.provide('pinger', new Service()) }
  `, (file, temporary) => {
    const { rust } = generate(file, root, { root: temporary })
    assert.match(rust, /::rutis_interop::npm_root\(".*generator-[^"]*"\)/)
    assert.match(rust, /\.join\("plugin\.ts"\)/)
    assert.match(rust, /\.join\("\.\.\/\.\.\/?"\)/)
    // No absolute path but the build-time default of the root itself.
    assert.equal(rust.split(temporary).length - 1, 1)
  })
})

test('service proxies keep their names when another member reaches the same class first', async () => {
  // Binding alpha reaches Beta through a member before the beta service is
  // bound; the service still gets the plain name, the live object a suffix.
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    declare module '@deepseek-ai/cordis' { interface Context { alpha: Alpha, beta: Beta } }
    export class Beta { ping(): number { return 1 } }
    export class Alpha { peer(): Beta { return new Beta() } }
    export function apply(ctx: Context) { ctx.provide('alpha', new Alpha()); ctx.provide('beta', new Beta()) }
  `, file => {
    const { rust } = generate(file, root)
    assert.match(rust, /pub struct Beta \{\s*process:/)
    assert.match(rust, /pub struct BetaObject\(pub ::rutis_interop::ObjectRef\);/)
    assert.match(rust, /pub fn peer\(&self\) -> Result<BetaObject, ::rutis_interop::Error>/)
    assert.doesNotMatch(rust, /Beta2/)
  })
})

test('configuration uses the input type of a declared Config schema', async () => {
  // A Schema is callable with its input and returns the resolved config; the
  // plugin receives the resolved type, the rutis side builds the input.
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    interface Schema<S, T> { (data?: S | null): T }
    declare class Volatile<T> { get(): T }
    declare module '@deepseek-ai/cordis' { interface Context { loop: Loop } }
    export default class Loop {
      static Config: Schema<{ max?: number, name?: string }, { max: Volatile<number>, name: string }>
      constructor(ctx: Context, config: { max: Volatile<number>, name: string }) { ctx.provide('loop', this) }
      run(): number { return 1 }
    }
  `, file => {
    const { rust } = generate(file, root)
    assert.match(rust, /pub struct Config \{[^}]*pub max: Option<f64>,[^}]*pub name: Option<String>,/)
    assert.doesNotMatch(rust, /Volatile/)
  })
})

test('a re-exported apply keeps its configuration type', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    declare module '@deepseek-ai/cordis' { interface Context { pinger: { ping(): number } } }
    export { apply } from './boot.ts'
  `, async (file, temporary) => {
    await writeFile(join(temporary, 'boot.ts'), `import type { Context } from '@deepseek-ai/cordis'
      export function apply(ctx: Context, config: { port?: number }) { ctx.provide('pinger', { ping: () => config.port ?? 0 }) }
    `)
    const { rust } = generate(file, root, { provide: ['pinger'] })
    assert.match(rust, /pub struct Config \{[^}]*pub port: Option<f64>,/)
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

// PR #73 review of 2fe5e3e: null stays apart from undefined, parameter
// names cannot shadow generated locals, and unions of live objects stay
// references.
test('null, parameter names and unions of live objects', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    declare module '@deepseek-ai/cordis' { interface Context { edge: Edge, edgeHost: EdgeHost } }
    export interface Input { key: string | null; note?: string; label?: string | null }
    export interface Config { key: string | null }
    export class Left { kind() { return 'left' } }
    export class Right { kind() { return 'right' } }
    export interface Picked { item: Left | { plain: true } }
    export interface EdgeHost { call(args: string[], suffix: string): string }
    export class Edge {
      nullable(value: string | null): boolean { return value === null }
      either(value?: string | null): string { return String(value) }
      input(input: Input): Input { return input }
      call(args: (value: number) => number): number { return args(1) }
      object(choice: boolean): Left | Right { return choice ? new Left() : new Right() }
      picked(): Picked { return { item: new Left() } }
    }
    export function apply(ctx: Context, config: Config) { ctx.provide('edge', new Edge()) }
  `, file => {
    const { rust, diagnostics } = generate(file, root, { provide: ['edgeHost'] })
    // Required nullable: None sends null. Optional and nullable: Option<Option<T>>.
    assert.match(rust, /pub fn nullable\(&self, value: Option<&str>\)/)
    assert.match(rust, /vec!\[::rutis_interop::arg\(&value\)\?\]/)
    assert.match(rust, /pub fn either\(&self, value: Option<Option<&str>>\)/)
    assert.match(rust, /vec!\[::rutis_interop::optional\(value\)\?\]/)
    assert.match(rust, /#\[serde\(rename = "key", default\)\] pub key: Option<String>,/)
    assert.match(rust, /#\[serde\(rename = "note", default, skip_serializing_if = "Option::is_none"\)\] pub note: Option<String>,/)
    assert.match(rust, /#\[serde\(rename = "label", default, skip_serializing_if = "Option::is_none", deserialize_with = "::rutis_interop::nullable"\)\] pub label: Option<Option<String>>,/)
    assert.match(rust, /pub struct Config \{ #\[serde\(rename = "key", default\)\] pub key: Option<String>, \}/)
    // A parameter named `args` does not shadow the generated locals.
    assert.match(rust, /let args = ::rutis_interop::rpc::Value::callback\(move \|__rutis_args\|/)
    assert.match(rust, /let args: Vec<String> = ::rutis_interop::decode_value\(__rutis_args\.next\(\)/)
    assert.match(rust, /let suffix: String = ::rutis_interop::decode_value\(__rutis_args\.next\(\)/)
    // A union of live objects is an untyped reference; each member keeps a proxy.
    assert.match(rust, /pub fn object\(&self, choice: bool\) -> Result<::rutis_interop::ObjectRef, ::rutis_interop::Error>/)
    assert.match(rust, /pub struct Left\(pub ::rutis_interop::ObjectRef\)/)
    assert.match(rust, /pub struct Right\(pub ::rutis_interop::ObjectRef\)/)
    // A union mixing live objects and data keeps the reference variant first.
    assert.deepEqual(diagnostics, [])
    assert.match(rust, /#\[serde\(untagged\)\]\npub enum PickedItem \{ Left\(Left\), PickedItem2\(PickedItem2\), \}/)
    assert.match(rust, /pub fn picked\(&self\) -> Result<Picked, ::rutis_interop::Error>/)
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
    assert.match(rust, /pub struct Config \{ pub clock: ClockConfig, #\[serde\(default\)\] pub greeter: GreeterConfig, \}/)
    // Configs deserialize, so a host can build them from JSON.
    assert.match(rust, /#\[derive\(Debug, Clone, ::rutis_interop::serde::Deserialize\)\]\s*#\[serde\(crate = "rutis_interop::serde"\)\]\s*pub struct Config/)
    assert.match(rust, /pub struct ClockConfig \{ #\[serde\(rename = "now"\)\] pub now: f64,/)
    assert.match(rust, /Process::mount\(/)
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
    assert.match(rust, /let changed = ::rutis_interop::rpc::Value::callback\(move \|__rutis_args\|/)
    assert.match(diagnostics.join('\n'), /hooks\.maybe is not bound: optional callback parameter/)
  })
})

test('selected notification events become rutis event types', async () => {
  await fixture(`import type { Context } from '@deepseek-ai/cordis'
    declare module '@deepseek-ai/cordis' {
      interface Events {
        'store/changed'(key: string, size?: number): void
        'store/decide'(key: string): boolean
      }
    }
    export function apply(ctx: Context) { ctx.provide('store', { ping(): number { return 1 } }) }
  `, file => {
    const { rust } = generate(file, root, { events: ['store/changed'] })
    assert.match(rust, /pub struct StoreChanged \{ pub key: String, pub size: Option<f64>, \}/)
    assert.match(rust, /impl ::rutis::Event for StoreChanged \{ const NAME: &'static str = "store\/changed"; type Value = \(\); \}/)
    assert.match(rust, /events\.forward::<StoreChanged>\("store\/changed", StoreChanged::from_args\);/)
    assert.throws(() => generate(file, root, { events: ['store/decide'] }), /event store\/decide is not a notification/)
    // rutis -> Cordis: the same event type gains to_args and a listener.
    const outward = generate(file, root, { emits: ['store/changed'] }).rust
    assert.match(outward, /pub fn to_args\(&self\) -> Result<Vec<::rutis_interop::rpc::Value>, ::rutis_interop::Error> \{\s*Ok\(vec!\[::rutis_interop::arg\(&self\.key\)\?, ::rutis_interop::optional\(self\.size\.as_ref\(\)\)\?\]\)/)
    assert.match(outward, /EmitToCordis::new\(process\.clone\(\), "store\/changed", StoreChanged::to_args\)/)
    assert.match(outward, /emits: vec!\["store\/changed"\.into\(\)\]/)
    assert.throws(() => generate(file, root, { events: ['store/changed'], emits: ['store/changed'] }), /selected in both directions/)
    assert.throws(() => generate(file, root, { events: ['store/missing'] }), /no Cordis Events declaration found/)
  })
})
