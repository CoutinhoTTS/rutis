export type MethodKind = 'sync' | 'async'

export interface Context {
  /** The service `name`: the object itself when a plugin in this process provides it, else a proxy. */
  use<T = any>(name: string): T
  /** Provide `value` as the service `name` until the plugin unloads, or until the returned function is called. */
  provide(name: string, value: unknown): () => unknown
  /** Run `cleanup` when the plugin unloads. */
  effect(cleanup: () => unknown): void
}

export interface PluginSpec<C = any> {
  name?: string
  /** Services the plugin uses: rutis starts it once they are all available. */
  inject?: string[]
  /** Services it provides to rutis, with each method's kind. */
  provides?: Record<string, Record<string, MethodKind>>
  /** The JSON Schema of its config. */
  config?: object
  apply(ctx: Context, config: C): void | (() => unknown) | Promise<void | (() => unknown)>
}

export declare const LEAF: unique symbol
export declare function definePlugin<C = any>(spec: PluginSpec<C>): PluginSpec<C>
export declare function isLeaf(value: unknown): boolean
