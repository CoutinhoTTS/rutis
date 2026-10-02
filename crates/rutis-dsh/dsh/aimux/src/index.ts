// The `llm-aimux` profile row: dsh-llm routes served by aimux, configured in
// the row's settings section (the web Models page edits it). Route changes
// apply live: the adapter is mounted again with the new route set.
import type { Context } from '@deepseek-ai/cordis'
import z from '@deepseek-ai/schemastery'
import * as adapter from './adapter.ts'
import type { Route } from './adapter.ts'

export const name = 'llm-aimux'
export const inject = ['llm', 'aimux']

const RouteSchema: z<Route> = z.object({
  provider: z.string(),
  apiKeyEnv: z.string().role('credential-ref'),
  displayName: z.string(),
})

export const Config = z.object({
  providers: z.dict(RouteSchema).default({}).volatile(),
})

interface Volatile<T> { get(): T }

export function apply(ctx: Context, config: { providers: Volatile<Record<string, Route>> }) {
  // The settings namespace is this row's id in the profile.
  const ns: string = (ctx.fiber as unknown as { entry?: { options: { id: string } } }).entry?.options.id ?? name
  const routes = (): Record<string, Route> => structuredClone(config.providers.get() ?? {})
  const entries = () => Object.entries(routes()).map(([provider, route]) => ({
    provider, displayName: route.displayName ?? provider, settingsNs: ns, settingsPath: ['providers', provider], declared: true,
  }))
  const directory = ctx.llm.registerConfigurableProviders(entries())
  let child = ctx.plugin(adapter, { providers: routes() })
  ctx.on('loader/volatile-update' as never, async () => {
    // An exception here would end the whole Node process: report it instead.
    try {
      directory.replace(entries())
      await child.dispose()
      child = ctx.plugin(adapter, { providers: routes() })
    } catch (error) {
      ctx.logger.error(error)
      console.error(`[llm-aimux] route update failed: ${error instanceof Error ? error.message : error}`)
    }
  })
}
