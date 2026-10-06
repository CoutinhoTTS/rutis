import { definePlugin } from '@arcships/rutis'

export interface Config {
  greeting?: string
}

class Greeter {
  constructor(private readonly greeting: string) {}

  hello(name: string): string {
    return `${this.greeting}, ${name}!`
  }
}

export default definePlugin<Config>({
  // Services this plugin uses: `inject: ['llm']`, then `ctx.use('llm')`.
  inject: [],
  // Services it provides, and whether each method is sync or async.
  provides: { greeter: { hello: 'sync' } },
  // The JSON Schema of its config.
  config: { type: 'object', properties: { greeting: { type: 'string', default: 'Hello' } } },
  apply(ctx, config) {
    ctx.provide('greeter', new Greeter(config.greeting ?? 'Hello'))
    // Return a cleanup, or use ctx.effect(cleanup), when the plugin holds
    // anything to release.
  },
})
