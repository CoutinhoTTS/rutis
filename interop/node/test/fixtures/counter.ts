import type { Context } from '@deepseek-ai/cordis'

declare module '@deepseek-ai/cordis' {
  interface Context { counter: Counter }
}

export interface Config { initial: number }

export class Counter {
  private value: number

  constructor(initial: number) { this.value = initial }

  add(amount: number): number {
    this.value += amount
    return this.value
  }

  current(): number { return this.value }

  async delayedAdd(amount: number): Promise<number> {
    await new Promise(resolve => setTimeout(resolve, 5))
    return this.add(amount)
  }

  fail(): number { throw new Error('counter refused operation') }
}

export function apply(ctx: Context, config: Config) {
  if (config.initial < 0) throw new Error('initial value must be non-negative')
  ctx.provide('counter', new Counter(config.initial))
}
