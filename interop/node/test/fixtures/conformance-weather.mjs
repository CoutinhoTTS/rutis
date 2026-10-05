// The runtime conformance plugin (rutis_interop::conformance::runtime), for
// the Node runtime.
export const inject = ['clock']
export function apply(ctx) {
  ctx.provide('weather', {
    today() { return `Oslo at ${ctx.clock.now()}` },
    async later() { return 'Oslo later' },
    each(callback) { return ['mon', 'tue'].map(day => callback(day)) },
    crash() { process.exit(17) },
  })
}
