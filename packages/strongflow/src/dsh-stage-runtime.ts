import type { Context } from '@deepseek-ai/cordis'
import type { Agent, AgentHandle } from '@deepseek-ai/dsh-agent'
import { createUserMessage } from '@deepseek-ai/dsh-llm'
import { SessionId } from '@deepseek-ai/dsh-session'
import type {
  FrozenDeliveryCandidate,
  RuntimeEvent,
} from '@winwincode/contracts'

import type {
  OpenStrongFlowStageRoleSessionInput,
  StrongFlowStageRoleSession,
  StrongFlowStageRuntime,
} from './delivery-stage-coordinator.js'

interface StrongFlowRuntimeSessionManifest {
  readonly dshSessionId: string
  readonly roleId: string
  readonly cwd: string
  readonly kernelSessionId: string
  readonly provider: string
  readonly model: string
}

/** Structural boundary supplied by @winwincode/dsh-profile without a package cycle. */
export interface StrongFlowDshAgentFactoryPort {
  readRuntimeSessionEvents(dshSessionId: string): Promise<readonly RuntimeEvent[]>
  readRuntimeSessionManifest(dshSessionId: string): Promise<StrongFlowRuntimeSessionManifest>
  reconcileDelivery(
    deliveryId: string,
    candidate?: FrozenDeliveryCandidate | null,
  ): ReturnType<StrongFlowStageRuntime['reconcileDelivery']>
}

export interface DshStrongFlowStageRuntimeOptions {
  readonly ctx: Context
  readonly agentFactory: StrongFlowDshAgentFactoryPort
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function ledgerMissing(error: unknown): boolean {
  return isRecord(error) && error.code === 'LEDGER_NOT_FOUND'
}

function signalOptions(signal: AbortSignal | undefined): { readonly signal?: AbortSignal } {
  return signal === undefined ? {} : { signal }
}

function agentOptions(input: OpenStrongFlowStageRoleSessionInput) {
  return Object.freeze({
    provider: input.modelRoute.provider,
    model: input.modelRoute.model,
    ...(input.modelRoute.maxTokens === undefined
      ? {}
      : { maxTokens: input.modelRoute.maxTokens }),
  })
}

function assertManifest(
  manifest: StrongFlowRuntimeSessionManifest,
  input: OpenStrongFlowStageRoleSessionInput,
): void {
  if (manifest.dshSessionId !== input.dshSessionId
    || manifest.roleId !== input.role
    || manifest.cwd !== input.cwd) {
    throw new Error(
      `DSH Session ${input.dshSessionId} does not match its StrongFlow role workspace`,
    )
  }
}

class DshStrongFlowRoleSession implements StrongFlowStageRoleSession {
  readonly dshSessionId: string
  readonly codexSessionId: string

  readonly #agent: Agent
  readonly #handle: AgentHandle
  #disposed = false

  constructor(handle: AgentHandle, codexSessionId: string) {
    this.#handle = handle
    this.#agent = handle.agent
    this.dshSessionId = handle.agent.id
    this.codexSessionId = codexSessionId
  }

  async turn(prompt: string, signal?: AbortSignal): Promise<void> {
    if (this.#disposed) throw new Error(`DSH Session ${this.dshSessionId} is disposed`)
    signal?.throwIfAborted()
    const cancel = (): void => { this.#agent.cancel({ kind: 'user' }) }
    signal?.addEventListener('abort', cancel, { once: true })
    try {
      this.#agent.followup(createUserMessage({
        content: [{ type: 'text', text: prompt }],
        source: { kind: 'plugin', plugin: '@winwincode/strongflow' },
      }))
      await this.#agent.whenIdle()
      signal?.throwIfAborted()
    } finally {
      signal?.removeEventListener('abort', cancel)
    }
  }

  async dispose(): Promise<void> {
    if (this.#disposed) return
    this.#disposed = true
    try {
      await this.#agent.ctx.sessions.flush(this.#agent.session)
    } finally {
      await this.#handle.dispose()
    }
  }
}

/**
 * DSH lifecycle adapter for StrongFlow role Sessions. It only creates, resumes,
 * drives, and disposes the registered embedded-Codex Agent; execution remains
 * inside the canonical WinWinCode AgentFactory.
 */
export class DshStrongFlowStageRuntime implements StrongFlowStageRuntime {
  readonly #ctx: Context
  readonly #factory: StrongFlowDshAgentFactoryPort

  constructor(options: DshStrongFlowStageRuntimeOptions) {
    if (typeof options?.ctx?.agents?.create !== 'function'
      || typeof options.ctx.agents.resume !== 'function'
      || typeof options.agentFactory?.readRuntimeSessionEvents !== 'function'
      || typeof options.agentFactory.readRuntimeSessionManifest !== 'function'
      || typeof options.agentFactory.reconcileDelivery !== 'function') {
      throw new TypeError('DSH StrongFlow stage runtime options are invalid')
    }
    this.#ctx = options.ctx
    this.#factory = options.agentFactory
  }

  reconcileDelivery(
    deliveryId: string,
    candidate: FrozenDeliveryCandidate | null,
  ): ReturnType<StrongFlowStageRuntime['reconcileDelivery']> {
    return this.#factory.reconcileDelivery(deliveryId, candidate)
  }

  readRuntimeSessionEvents(dshSessionId: string): Promise<readonly RuntimeEvent[]> {
    return this.#factory.readRuntimeSessionEvents(dshSessionId)
  }

  async openRoleSession(
    input: OpenStrongFlowStageRoleSessionInput,
  ): Promise<StrongFlowStageRoleSession> {
    input.signal?.throwIfAborted()
    const sessionId = SessionId(input.dshSessionId)
    if (this.#ctx.agents.get(sessionId) !== undefined) {
      throw new Error(`DSH Session ${input.dshSessionId} is already live`)
    }

    let persisted: StrongFlowRuntimeSessionManifest | null
    try {
      persisted = await this.#factory.readRuntimeSessionManifest(input.dshSessionId)
      assertManifest(persisted, input)
    } catch (error) {
      if (!ledgerMissing(error)) throw error
      persisted = null
    }

    const handle = persisted === null
      ? await this.#ctx.agents.create({
        sessionId,
        meta: { cwd: input.cwd, agentPreset: input.role },
        agentOptions: agentOptions(input),
        ...signalOptions(input.signal),
      })
      : await this.#ctx.agents.resume({
        resumeSessionId: sessionId,
        agentOptions: agentOptions(input),
        ...signalOptions(input.signal),
      })
    try {
      const manifest = await this.#factory.readRuntimeSessionManifest(input.dshSessionId)
      assertManifest(manifest, input)
      return new DshStrongFlowRoleSession(handle, manifest.kernelSessionId)
    } catch (error) {
      await handle.dispose()
      throw error
    }
  }
}
