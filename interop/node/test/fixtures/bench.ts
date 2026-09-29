import type { Context } from '@deepseek-ai/cordis'

declare module '@deepseek-ai/cordis' {
  interface Context { bench: Bench }
}

export interface Config {}

export interface Row { id: number, name: string, tags: string[], score: number }

export class Item {
  constructor(private value: number) {}
  get current(): number { return this.value }
  add(amount: number): number { return this.value += amount }
}

/** Call shapes used to measure the cost of crossing the process boundary. */
export class Bench {
  private shared = new Item(0)
  noop(): number { return 0 }
  async noopAsync(): Promise<number> { return 0 }
  echo(text: string): string { return text }
  rows(count: number): Row[] {
    return Array.from({ length: count }, (_, id) => ({ id, name: `row-${id}`, tags: ['a', 'b'], score: id / 2 }))
  }
  take(rows: Row[]): number { return rows.length }
  item(): Item { return this.shared }
  each(count: number, visit: (index: number) => void): number {
    for (let index = 0; index < count; index++) visit(index)
    return count
  }
}

export function apply(ctx: Context) {
  ctx.provide('bench', new Bench())
}
