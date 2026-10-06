# @arcships/rutis

Write [rutis](https://github.com/arcships/rutis) plugins in TypeScript or JavaScript.

```ts
import { definePlugin } from '@arcships/rutis'

export default definePlugin({
  inject: ['llm'],                              // services it uses
  provides: { weather: { today: 'async' } },    // services it provides
  config: { type: 'object', properties: { city: { type: 'string' } } },
  apply(ctx, config) {
    ctx.provide('weather', new Weather(ctx.use('llm'), config.city))
    return () => {}                             // cleanup
  },
})
```

Test it without a host:

```ts
import { load } from '@arcships/rutis/testing'

const t = await load(plugin, { config: { city: 'Oslo' }, services: { llm: { ask: async () => 'sunny' } } })
assert.equal(await t.service('weather').today(), 'sunny in Oslo')
await t.unload()
```

This package has no dependencies; the host installs the runtime that runs the plugin (`@arcships/rutis-runtime`). Start a project with `npx @arcships/rutis-host new <name> --lang node`.

Guide (Chinese): [docs/guide/typescript-plugin.md](https://github.com/arcships/rutis/blob/main/docs/guide/typescript-plugin.md).
