// A packaged-style plugin: a default-exported Service class. Its methods
// create effects through `this.ctx`, and Service.check gates readiness.
import { Service } from '@deepseek-ai/cordis'

export default class Ticker extends Service {
  constructor(ctx, config = {}) {
    super(ctx, 'ticker')
    this.ready = config.ready ?? true
    this.ticks = 0
  }

  [Service.check]() { return this.ready }

  start() {
    this.ctx.effect(() => {
      const timer = setInterval(() => { this.ticks++ }, 5)
      return () => clearInterval(timer)
    })
    return true
  }

  count() { return this.ticks }
}
