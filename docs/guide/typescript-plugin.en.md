# Write a TypeScript Plugin

This guide covers creating a project, publishing it, and using it from a host. JavaScript uses the same approach without the types.

## 1. Create a project

```bash
npx @arcships/rutis-host new greeter --lang node
cd greeter
npm install
```

The generated project contains:

| File | Purpose |
| --- | --- |
| `src/index.ts` | The plugin itself. |
| `test/index.test.ts` | Unit tests that do not need a host. |
| `rutis.dev.json` | Local runtime configuration and other plugins used for testing. |
| `package.json` | Depends on `@arcships/rutis`; development dependencies include `@arcships/rutis-host` and `@arcships/rutis-runtime`. |
| `.github/workflows/publish.yml` | Tests and publishes to npm when a `v*` tag is pushed. |

## 2. Write the plugin

```ts
import { definePlugin } from '@arcships/rutis'

export interface Config {
  city?: string
}

interface Llm {
  ask(question: string): Promise<string>
}

class Weather {
  constructor(private readonly llm: Llm, private readonly city: string) {}

  async today(): Promise<string> {
    return `${await this.llm.ask(`weather in ${this.city}`)} in ${this.city}`
  }
}

export default definePlugin<Config>({
  inject: ['llm'],                                 // Services used: start when all are ready; stop if any is revoked
  provides: { weather: { today: 'async' } },       // Services provided, with each method marked sync or async
  config: { type: 'object', properties: { city: { type: 'string' } } },  // Configuration JSON Schema
  apply(ctx, config) {
    ctx.provide('weather', new Weather(ctx.use<Llm>('llm'), config.city ?? 'Oslo'))
    return () => {}                                // Cleanup; ctx.effect(cleanup) also works, or return nothing
  },
})
```

The plugin depends only on `@arcships/rutis`. It does not need to know which host runs it, which language or machine provides `llm`, or when it starts and stops; the host decides those things. See [Plugin API](plugin-api.en.md) for capabilities and value passing rules.

## 3. Test

`@arcships/rutis/testing` provides fake services and calls the services provided by the plugin without requiring a host:

```ts
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { load } from '@arcships/rutis/testing'
import plugin from '../src/index.ts'

test('reports the weather of the configured city', async () => {
  const t = await load(plugin, {
    config: { city: 'Oslo' },
    services: { llm: { ask: async () => 'sunny' } },
  })
  assert.equal(await t.service('weather').today(), 'sunny in Oslo')
  await t.unload()
})
```

```bash
npm test
```

`load` checks the same things as the host: the plugin uses only services declared in `inject`; its provided service has every method declared in `provides`; and all cleanup functions run when it unloads. In strict mode (the default), values follow cross-process rules: data is copied, functions and objects with methods are passed by reference, and a method declared `sync` cannot return a Promise. Code that works only within one process fails here.

## 4. Run in a local host

```bash
npx rutis-host dev
```

`dev` runs this project as a row using the source file `src/index.ts`, without a build, and reloads it when files change. Add other required services to `rutis.dev.json`, such as a fake `llm`:

```json
{
  "rows": [
    { "id": "greeter", "config": { "city": "Oslo" } },
    { "id": "llm", "name": "./dev/fake-llm.ts" }
  ]
}
```

```ts
// dev/fake-llm.ts
import { definePlugin } from '@arcships/rutis'
export default definePlugin({
  provides: { llm: { ask: 'async' } },
  apply(ctx) { ctx.provide('llm', { ask: async () => 'sunny' }) },
})
```

The row with the same ID as the plugin supplies its configuration; the other rows are additional plugins to run. `npx rutis-host check` lists each row's dependencies, provided services, and configuration schema, and exits with a nonzero status if there is a problem. It is suitable for CI.

## 5. Publish

```bash
npm run build      # tsc generates dist/
npm publish
```

Alternatively, push a tag such as `v0.1.0`. The generated workflow tests, runs `check`, and publishes (the repository needs an `NPM_TOKEN` secret).

Declare a version range such as `"@arcships/rutis": "^0.7.0"`. The plugin is tagged with its plugin API version (`definePlugin` adds this automatically). If the host runtime is older, it reports the problem clearly instead of failing later during execution.

## 6. Use from a host

Install the plugin in the host's Node project, alongside `@arcships/rutis-runtime`, then add a row named after the package:

```bash
npm install greeter
```

```json
{ "id": "greeter", "name": "greeter", "config": { "city": "Oslo" } }
```

See [rutis-host and rutis.json](rutis-host.en.md), or [Embed in a Rust application](rust-host.en.md) for a Rust host.

## Existing Cordis plugins

No rewrite is needed. Declare the services rutis should expose in the plugin package's `package.json`:

```json
"rutis": { "provides": { "weather": { "today": "async" } } }
```

The plugin runs in the Node runtime's Cordis Context, in the same process as leaf plugins. They receive the actual objects when using each other's services.
