// The node conformance suite's hosted plugin: records through `probe` when
// it starts and when it stops.
export const name = 'conformance-greeter'
export const inject = ['probe']
export function apply(ctx, config) {
  ctx.probe.record(`greeter: ${config.text}`)
  ctx.effect(() => () => ctx.probe.record(`greeter gone: ${config.text}`))
}
