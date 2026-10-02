// Test probe: consumes `ctx.llm.stream` the way a dsh caller does, so the
// rutis side can check what dsh sees.
import type { Context } from '@deepseek-ai/cordis'

declare module '@deepseek-ai/cordis' {
  interface Context { llmProbe: LlmProbe }
}

export class LlmProbe {
  constructor(private readonly ctx: Context) {}

  /** Stream one call and return every chunk as JSON, or the thrown error as `{ thrown }`. */
  async collect(provider: string, model: string, system: string, prompt: string): Promise<string[]> {
    const chunks: string[] = []
    try {
      for await (const chunk of this.ctx.llm.stream({
        provider, model, system,
        messages: [{ role: 'user', content: [{ type: 'text', text: prompt }] }],
        tools: [{ name: 'lookup', description: 'Look something up', parameters: { type: 'object' } }],
      })) chunks.push(JSON.stringify(chunk))
    } catch (error) {
      chunks.push(JSON.stringify({ thrown: { name: (error as Error).name, code: (error as { code?: string }).code, message: (error as Error).message } }))
    }
    return chunks
  }

  /** Stream until the first chunk arrives, then stop reading. */
  async firstChunk(provider: string, model: string): Promise<string> {
    for await (const chunk of this.ctx.llm.stream({ provider, model, messages: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }] })) {
      return JSON.stringify(chunk)
    }
    return ''
  }
}

export const inject = ['llm']

export function apply(ctx: Context) {
  ctx.provide('llmProbe', new LlmProbe(ctx))
}
