// dsh-agent-loop with its default configuration. Its config type is the
// resolved schema output (live `Volatile` values), which cannot be built on
// the rutis side, so it is mounted from here.
import type { Context } from '@deepseek-ai/cordis'
import AgentLoop from '@deepseek-ai/dsh-agent-loop'

export function apply(ctx: Context) {
  ctx.plugin(AgentLoop, {})
}
