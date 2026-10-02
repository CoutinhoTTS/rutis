/// <reference types="node" />
// Serves dsh-llm model calls from aimux-llm, which the rutis host provides as
// `ctx.aimux`. This is the only code that knows both shapes: dsh requests and
// chunks on one side, aimux's neutral parts on the other.
import { env } from 'node:process'
import type { Context } from '@deepseek-ai/cordis'
import { LlmAdapter, LlmError, ToolCallId, type GenerateOptions, type StreamChunk } from '@deepseek-ai/dsh-llm'

declare module '@deepseek-ai/cordis' {
  interface Context { aimux: Aimux }
}

/** One model call in the neutral shape aimux-llm accepts. */
export interface AimuxRequest {
  provider: string
  model: string
  apiKey?: string
  system?: string
  messages: AimuxMessage[]
  tools: AimuxTool[]
}

export interface AimuxMessage {
  role: string
  text: string
  /** The tool calls an assistant message made. */
  toolCalls?: AimuxToolCall[]
  /** For a tool message: the call it answers. */
  toolCallId?: string
  toolName?: string
  isError?: boolean
}

export interface AimuxToolCall { id: string, name: string, arguments: string }

export interface AimuxTool { name: string, description?: string, parameters?: unknown }

/** One neutral stream part; `kind` says which fields are set. */
export interface AimuxPart {
  kind: 'text' | 'reasoning' | 'tool-call' | 'finish' | 'error'
  /** text, reasoning */
  delta?: string
  /** tool-call */
  id?: string
  name?: string
  arguments?: string
  /** finish */
  reason?: 'stop' | 'tool-calls' | 'length' | 'other'
  inputTokens?: number
  outputTokens?: number
  cacheReadTokens?: number
  cacheWriteTokens?: number
  /** error */
  code?: string
  message?: string
}

/** The aimux model service, implemented by the rutis host. */
export interface Aimux {
  /** Start a call; its failure arrives as an `error` part. */
  open(request: AimuxRequest): Promise<string>
  /** The parts produced since the last read, waiting for at least one; empty once the call ended. */
  next(stream: string): Promise<AimuxPart[]>
  /** Stop a call and release it. */
  close(stream: string): void
  listModels(provider: string, apiKey?: string): Promise<string[]>
}

/** One dsh-llm provider route served by aimux. */
export interface Route {
  /** The aimux provider; defaults to the route name. */
  provider?: string
  /** Credential reference of the provider key, resolved per request. */
  apiKeyEnv?: string
  displayName?: string
}

export interface Config {
  /** dsh-llm provider routes by name. Without a key the aimux host uses its own. */
  providers: Record<string, Route>
}

export const inject = ['llm', 'aimux']

interface Credentials { resolve(ref: string): Promise<{ value: string } | undefined> }

function request(options: GenerateOptions, provider: string, apiKey: string | undefined): AimuxRequest {
  const blocks = (content: unknown): any[] => Array.isArray(content) ? content : []
  // Visible text only: reasoning blocks are the model's own scratch.
  const text = (content: unknown) => blocks(content).filter(block => block?.type === 'text').map(block => block.text).join('')
  // Tool messages carry only the call id; the name comes from the call.
  const names = new Map<string, string>()
  const message = (message: GenerateOptions['messages'][number]): AimuxMessage => {
    const role = message.role ?? 'user'
    if (role === 'assistant') {
      const toolCalls = blocks(message.content)
        .filter(block => block?.type === 'tool-call')
        .map(block => ({ id: String(block.id), name: block.name, arguments: block.arguments }))
      for (const call of toolCalls) names.set(call.id, call.name)
      return { role, text: text(message.content), ...(toolCalls.length ? { toolCalls } : {}) }
    }
    if (role === 'tool' && 'toolCallId' in message) {
      const toolCallId = String(message.toolCallId)
      const toolName = names.get(toolCallId)
      return {
        role, text: text(message.content), toolCallId,
        ...(toolName !== undefined ? { toolName } : {}),
        isError: message.isError === true,
      }
    }
    return { role, text: text(message.content) }
  }
  return {
    provider,
    model: options.model,
    ...(apiKey !== undefined ? { apiKey } : {}),
    ...(options.system !== undefined ? { system: options.system } : {}),
    messages: options.messages.map(message),
    tools: (options.tools ?? []).map(tool => ({
      name: tool.name,
      ...(tool.description !== undefined ? { description: tool.description } : {}),
      ...(tool.parameters !== undefined ? { parameters: tool.parameters } : {}),
    })),
  }
}

/** Turns neutral parts into dsh chunks, giving every content block its own index. */
class Chunks {
  private index = -1
  private open: { kind: 'text' | 'reasoning', text: string } | undefined

  *map(part: AimuxPart): Iterable<StreamChunk> {
    switch (part.kind) {
      case 'text':
      case 'reasoning': {
        if (this.open?.kind !== part.kind) {
          yield* this.close()
          this.open = { kind: part.kind, text: '' }
          yield { type: 'block-start', index: ++this.index, blockType: part.kind }
        }
        this.open.text += part.delta ?? ''
        yield { type: part.kind === 'text' ? 'text-delta' : 'reasoning-delta', index: this.index, text: part.delta ?? '' }
        return
      }
      case 'tool-call': {
        yield* this.close()
        const id = ToolCallId(part.id ?? '')
        const name = part.name ?? ''
        const args = part.arguments ?? ''
        const index = ++this.index
        yield { type: 'block-start', index, blockType: 'tool-call' }
        yield { type: 'tool-call-delta', index, id, name, argumentsDelta: args }
        yield { type: 'block-end', index, block: { type: 'tool-call', id, name, arguments: args } }
        return
      }
      case 'finish': {
        yield* this.close()
        yield {
          type: 'usage',
          usage: {
            inputTokens: part.inputTokens ?? 0,
            outputTokens: part.outputTokens ?? 0,
            ...(part.cacheReadTokens !== undefined ? { cacheReadTokens: part.cacheReadTokens } : {}),
            ...(part.cacheWriteTokens !== undefined ? { cacheWriteTokens: part.cacheWriteTokens } : {}),
          },
        }
        const kind = part.reason === 'tool-calls' ? 'tool-calls' : part.reason === 'length' ? 'max-tokens' : 'stop'
        yield { type: 'finish', reason: { kind } }
        return
      }
      case 'error':
        throw new LlmError(part.message ?? 'aimux stream failed', part.code ?? 'PROVIDER')
    }
  }

  private *close(): Iterable<StreamChunk> {
    if (!this.open) return
    yield { type: 'block-end', index: this.index, block: { type: this.open.kind, text: this.open.text } }
    this.open = undefined
  }
}

class AimuxAdapter extends LlmAdapter {
  constructor(private readonly ctx: Context, private readonly routes: Record<string, Route>) { super() }

  providerInfo(provider: string) {
    return { id: provider, name: this.routes[provider]?.displayName ?? provider }
  }

  /** The route's aimux provider and key: a credential when `credentials` is mounted, else the environment. */
  private async backend(route: string): Promise<[string, string | undefined]> {
    const { provider = route, apiKeyEnv } = this.routes[route] ?? {}
    if (apiKeyEnv === undefined) return [provider, undefined]
    const credentials = this.ctx.get('credentials') as Credentials | undefined
    const apiKey = credentials ? (await credentials.resolve(apiKeyEnv))?.value : env[apiKeyEnv]
    return [provider, apiKey]
  }

  async listModels(route: string) {
    const [provider, apiKey] = await this.backend(route)
    const models = await this.ctx.aimux.listModels(provider, apiKey)
    return models.map(id => ({ provider: route, id, name: id }))
  }

  async *stream(options: GenerateOptions): AsyncIterable<StreamChunk> {
    const aimux = this.ctx.aimux
    const [provider, apiKey] = await this.backend(options.provider)
    const stream = await aimux.open(request(options, provider, apiKey))
    const abort = () => aimux.close(stream)
    options.signal?.addEventListener('abort', abort, { once: true })
    try {
      const chunks = new Chunks()
      while (!options.signal?.aborted) {
        const parts = await aimux.next(stream)
        if (!parts.length) return
        for (const part of parts) yield* chunks.map(part)
      }
    } finally {
      options.signal?.removeEventListener('abort', abort)
      aimux.close(stream)
    }
  }
}

export function apply(ctx: Context, config: Config) {
  const routes = Object.keys(config.providers)
  if (routes.length) ctx.llm.registerAdapter(routes, new AimuxAdapter(ctx, config.providers))
}
