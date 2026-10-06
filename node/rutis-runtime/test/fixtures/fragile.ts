import type { Context } from '@deepseek-ai/cordis'

// No @types/node in this package: the one global used here.
declare const process: { pid: number, exit(code: number): never }

declare module '@deepseek-ai/cordis' {
  interface Context { fragile: Fragile }
}

export interface Config {
  /** Exit with this code while the plugin is being applied. */
  exitOnApply?: number
}

/** Ways a Cordis plugin can take its Node process down or stall it. */
export class Fragile {
  pid(): number { return process.pid }
  ping(): number { return 1 }
  /** process.exit inside a synchronous call. */
  exit(code: number): number { return process.exit(code) }
  /** An exception thrown from a timer, outside any call. */
  throwLater(): number { setTimeout(() => { throw new Error('thrown from a timer') }); return 0 }
  /** A rejected Promise nobody handles. */
  rejectLater(): number { Promise.reject(new Error('unhandled rejection')); return 0 }
  /** Blocks the event loop. */
  spin(ms: number): number { const end = Date.now() + ms; while (Date.now() < end); return ms }
  /** Never settles. */
  async hang(): Promise<number> { return new Promise(() => {}) }
  async later(ms: number): Promise<number> { await new Promise(done => setTimeout(done, ms)); return ms }
}

export function apply(ctx: Context, config: Config) {
  if (config.exitOnApply !== undefined) process.exit(config.exitOnApply)
  ctx.provide('fragile', new Fragile())
}
