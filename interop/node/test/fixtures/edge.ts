import type { Context } from '@deepseek-ai/cordis'

// Edge cases of the generated bindings (PR #73 review of 2fe5e3e): null
// versus undefined, parameter names that match generated locals, and unions
// of live objects.
declare module '@deepseek-ai/cordis' {
  interface Context { edge: Edge, edgeHost: Joiner }
}

export interface Config { key: string | null }

export interface Input { key: string | null; note?: string; label?: string | null }

export interface Joiner { call(args: string[], suffix: string): string }

export interface Label { text: string }

export class Left { kind(): string { return 'left' } }
export class Right { kind(): string { return 'right' } }

const describe = (value: unknown) => value === null ? 'null' : value === undefined ? 'undefined' : String(value)

export class Edge {
  constructor(private ctx: Context, private config: Config) {}
  nullable(value: string | null): string { return describe(value) }
  either(value?: string | null): string { return describe(value) }
  input(input: Input): string {
    const field = (key: keyof Input) => key in input ? describe(input[key]) : 'missing'
    return [field('key'), field('note'), field('label')].join(',')
  }
  configKey(): string { return describe(this.config.key) }
  call(args: (value: number) => number): number { return args(20) + 1 }
  host(): string { return this.ctx.edgeHost.call(['a', 'b'], '!') }
  object(choice: boolean): Left | Right { return choice ? new Left() : new Right() }
  dynamic(): unknown { return { item: new Left() } }
  pick(choice: boolean): Left | Label { return choice ? new Left() : { text: 'label' } }
  describeItem(item: Left | string): string { return item instanceof Left ? `object ${item.kind()}` : `text ${item}` }
}

export const inject = ['edgeHost']

export function apply(ctx: Context, config: Config) {
  ctx.provide('edge', new Edge(ctx, config))
}
