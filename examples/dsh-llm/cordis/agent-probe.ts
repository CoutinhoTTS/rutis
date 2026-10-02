// Test probe: runs one dsh agent turn with a `lookup` tool, so the rutis side
// can check the whole loop over models served by aimux.
import type { Context } from '@deepseek-ai/cordis'
import { createUserMessage } from '@deepseek-ai/dsh-llm'
import { SessionId } from '@deepseek-ai/dsh-session'
import { defineTool } from '@deepseek-ai/dsh-tools'

declare module '@deepseek-ai/cordis' {
  interface Context { agentProbe: AgentProbe }
}

export class AgentProbe {
  private runs = 0

  constructor(private readonly ctx: Context) {}

  /** Run one turn to idle and return the session's messages as JSON. */
  async run(provider: string, model: string, prompt: string): Promise<string[]> {
    const handle = await this.ctx.agents.create({
      sessionId: SessionId(`probe-${++this.runs}`),
      agentOptions: { provider, model },
    })
    try {
      handle.agent.followup(createUserMessage({ content: [{ type: 'text', text: prompt }], source: { kind: 'user' } }))
      await handle.agent.whenIdle()
      return handle.agent.session.deriveMessages().map(message => JSON.stringify({
        role: message.role,
        content: message.content,
        ...('isError' in message ? { isError: message.isError } : {}),
      }))
    } finally {
      await handle.dispose()
    }
  }
}

export const inject = ['agents', 'tools']

export function apply(ctx: Context) {
  ctx.tools.register(defineTool({
    name: 'lookup',
    description: 'Look up a code name.',
    parameters: { q: { type: 'string', required: true } },
    output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
    async execute(args) { return `RESULT:${args.q}` },
  }))
  ctx.provide('agentProbe', new AgentProbe(ctx))
}
