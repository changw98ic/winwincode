import { randomUUID } from 'node:crypto'
import { isAbsolute } from 'node:path'

import { Service, type Context } from '@deepseek-ai/cordis'
import {
  Inbox,
  agentEvents,
  assembleContextFor,
  emitAgentEvent,
  type Agent,
  type AgentCancelCause,
  type AgentFactory,
  type AgentHandle,
  type AgentOptions,
  type AgentSetup,
  type AgentStatus,
  type CancelOptions,
  type CreateAgentOptions,
  type InboxTarget,
  type ResumeAgentOptions,
  type SessionStartSource,
} from '@deepseek-ai/dsh-agent'
import {
  CallId,
  type LlmCallConfig,
  type UserMessage,
} from '@deepseek-ai/dsh-llm'
import { createScope, type Scope } from '@deepseek-ai/dsh-scope'
import {
  canonicalHeader,
  headerEquals,
  type EpochHeader,
  type Session,
  type SessionEvent,
  type SessionEventMap,
  type SessionId,
  type SessionPreparation,
} from '@deepseek-ai/dsh-session'
import type { ApprovalOutcome } from '@deepseek-ai/dsh-user-approval'
import z from '@deepseek-ai/schemastery'
import {
  STRONGFLOW_ROLE_IDS,
  strongFlowRoleSessionPolicy,
  type FrozenDeliveryCandidate,
  type RuntimeEvent,
  type StrongFlowRoleId,
  type StrongFlowRoleSessionPolicy,
} from '@winwincode/contracts'
import {
  WinWinCodeKernel,
  type ApprovalResponse,
  type EventStreamOptions,
  type ForkOptions,
  type KernelEvent,
  type KernelOptions,
  type ResumeOptions,
  type SessionInfo,
  type SessionOptions,
  type ShutdownInfo,
  type SteerOptions,
  type SubmissionInfo,
} from '@winwincode/native'

import { DshModelPort, type DshLlmRuntime } from './model-port.js'
import {
  CodexRuntimeProjector,
  type KernelEventSource,
} from './runtime-events.js'
import {
  DshRuntimeProjection,
  RuntimeApprovalRouter,
  type DshSessionAppend,
} from './runtime-projection.js'
import {
  RuntimeSessionLedger,
  type RuntimeKernelLifecycle,
  type RuntimeSessionManifest,
} from './session-ledger.js'
import {
  reconcileDeliveryAfterRestart,
  type DeliveryRecoverySnapshot,
} from './delivery-recovery.js'

const DEFAULT_ROLE_ID = 'chat'

declare module '@deepseek-ai/cordis' {
  interface Context {
    winwincodeAgentFactory: WinWinCodeAgentFactory
  }
}

export type WinWinCodeAgentFactoryErrorCode =
  | 'INVALID_FACTORY_CONFIG'
  | 'MODEL_ROUTE_MISSING'
  | 'ROLLOUT_PATH_MISSING'
  | 'ROLE_ID_MISMATCH'
  | 'PROJECTION_HISTORY_DIVERGED'
  | 'MESSAGE_CONTENT_UNSUPPORTED'
  | 'KERNEL_TURN_NOT_STARTED'

/** Explicit startup or turn failure at the DSH-to-Codex execution boundary. */
export class WinWinCodeAgentFactoryError extends Error {
  readonly code: WinWinCodeAgentFactoryErrorCode

  constructor(code: WinWinCodeAgentFactoryErrorCode, message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'WinWinCodeAgentFactoryError'
    this.code = code
  }
}

/** Narrow port used by the Cordis factory and replaceable by deterministic tests. */
export interface EmbeddedKernelPort extends KernelEventSource {
  createSession(options: SessionOptions): Promise<SessionInfo>
  resumeSession(options: ResumeOptions): Promise<SessionInfo>
  forkSession(options: ForkOptions): Promise<SessionInfo>
  submitTurn(sessionId: string, text: string): Promise<SubmissionInfo>
  steer(options: SteerOptions): Promise<SubmissionInfo>
  interrupt(sessionId: string): Promise<string>
  resolveApproval(response: ApprovalResponse): Promise<string>
  listSessions(): Promise<readonly string[]>
  closeSession(sessionId: string): Promise<void>
  shutdown(): Promise<ShutdownInfo>
  events(sessionId: string, options?: EventStreamOptions): AsyncIterable<KernelEvent>
}

export type EmbeddedKernelFactory = (options: KernelOptions) => EmbeddedKernelPort

export interface Config {
  readonly home: string
  readonly roleId?: string
  readonly nativeDirectory?: string
  readonly eventCapacity?: number
  readonly shutdownTimeoutMillis?: number
}

interface ResolvedConfig {
  readonly home: string
  readonly roleId: string
  readonly nativeDirectory?: string
  readonly eventCapacity?: number
  readonly shutdownTimeoutMillis?: number
}

interface ModelRoute {
  readonly config: LlmCallConfig
  readonly provider: string
  readonly model: string
}

interface NativeRuntimeState extends RuntimeKernelLifecycle {
  readonly normalizedSequence: string
}

function nonEmpty(value: string | undefined, label: string): string {
  if (value === undefined || value.length === 0) {
    throw new WinWinCodeAgentFactoryError(
      'MODEL_ROUTE_MISSING',
      `${label} must be selected before starting an embedded Codex session`,
    )
  }
  return value
}

function sessionCwd(session: Session): string {
  const cwd = session.header.cwd ?? process.cwd()
  if (!isAbsolute(cwd)) {
    throw new WinWinCodeAgentFactoryError(
      'INVALID_FACTORY_CONFIG',
      `DSH session ${session.id} has a non-absolute cwd`,
    )
  }
  return cwd
}

function sessionRoleId(session: Session, defaultRoleId: string): string {
  const roleId = session.header.agentPreset ?? defaultRoleId
  if (roleId.length === 0) {
    throw new WinWinCodeAgentFactoryError(
      'INVALID_FACTORY_CONFIG',
      `DSH session ${session.id} resolved an empty WinWinCode role id`,
    )
  }
  return roleId
}

function roleSessionPolicy(roleId: string): StrongFlowRoleSessionPolicy | undefined {
  return STRONGFLOW_ROLE_IDS.includes(roleId as StrongFlowRoleId)
    ? strongFlowRoleSessionPolicy(roleId as StrongFlowRoleId)
    : undefined
}

function assertPersistedRole(session: Session, persistedRoleId: string): void {
  const requestedRoleId = session.header.agentPreset
  if (requestedRoleId !== undefined && requestedRoleId !== persistedRoleId) {
    throw new WinWinCodeAgentFactoryError(
      'ROLE_ID_MISMATCH',
      `DSH session ${session.id} requests role ${requestedRoleId} but its runtime ledger records ${persistedRoleId}`,
    )
  }
}

function initialRoute(options: AgentOptions): ModelRoute {
  const provider = nonEmpty(options.provider, 'provider')
  const model = nonEmpty(options.model, 'model')
  const config: LlmCallConfig = {
    provider,
    model,
    ...(options.maxTokens === undefined ? {} : { maxTokens: options.maxTokens }),
  }
  return Object.freeze({ config: Object.freeze(config), provider, model })
}

function rolloutPath(info: SessionInfo): string {
  if (info.rolloutPath === undefined || info.rolloutPath.length === 0) {
    throw new WinWinCodeAgentFactoryError(
      'ROLLOUT_PATH_MISSING',
      `embedded Codex session ${info.sessionId} did not return a rollout path`,
    )
  }
  return info.rolloutPath
}

function textFromMessages(messages: readonly UserMessage[]): string {
  const parts: string[] = []
  for (const message of messages) {
    for (const block of message.content) {
      if (block.type === 'text') parts.push(block.text)
      else {
        throw new WinWinCodeAgentFactoryError(
          'MESSAGE_CONTENT_UNSUPPORTED',
          `embedded Codex text submission does not accept DSH content block ${block.type}`,
        )
      }
    }
  }
  const text = parts.join('\n').trim()
  if (text.length === 0) {
    throw new WinWinCodeAgentFactoryError(
      'MESSAGE_CONTENT_UNSUPPORTED',
      'embedded Codex submission contains no text',
    )
  }
  return text
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

interface Deferred<T> {
  readonly promise: Promise<T>
  readonly resolve: (value: T | PromiseLike<T>) => void
  readonly reject: (reason?: unknown) => void
}

function deferred<T>(): Deferred<T> {
  let resolvePromise: Deferred<T>['resolve'] = () => {}
  let rejectPromise: Deferred<T>['reject'] = () => {}
  const promise = new Promise<T>((resolve, reject) => {
    resolvePromise = resolve
    rejectPromise = reject
  })
  return { promise, resolve: resolvePromise, reject: rejectPromise }
}

const PROJECTION_EVENT_TYPES: ReadonlySet<string> = new Set([
  'turn/start',
  'turn/end',
  'step/start',
  'step/end',
  'user/message',
  'assistant/chunk',
  'assistant/message',
  'tool/call',
  'tool/result',
])

function projectionKey(append: Pick<DshSessionAppend, 'type' | 'data'>): string {
  return `${append.type}\u0000${JSON.stringify(append.data)}`
}

/** Typed writer that preserves DSH turn/step and surface invariants. */
class DshSessionWriter {
  readonly #session: Session
  readonly #chunkSeqs: number[] = []

  constructor(session: Session) {
    this.#session = session
    let stepOpen = false
    for (const event of session.events) {
      if (event.type === 'step/start') {
        stepOpen = true
        this.#chunkSeqs.length = 0
      } else if (event.type === 'assistant/chunk' && stepOpen) {
        this.#chunkSeqs.push(event.seq)
      } else if (event.type === 'step/end') {
        stepOpen = false
        this.#chunkSeqs.length = 0
      }
    }
  }

  append(append: DshSessionAppend): void {
    switch (append.type) {
      case 'turn/start':
        this.#session.append('turn/start', append.data as SessionEventMap['turn/start'])
        return
      case 'turn/end':
        this.#session.append('turn/end', append.data as SessionEventMap['turn/end'])
        return
      case 'step/start':
        this.#chunkSeqs.length = 0
        this.#session.append('step/start', append.data as SessionEventMap['step/start'])
        return
      case 'step/end':
        this.#session.append('step/end', append.data as SessionEventMap['step/end'])
        this.#chunkSeqs.length = 0
        return
      case 'user/message':
        this.#session.append(
          'user/message',
          append.data as unknown as SessionEventMap['user/message'],
          { surfaceOp: 'append' },
        )
        return
      case 'assistant/chunk': {
        const event = this.#session.append(
          'assistant/chunk',
          append.data as SessionEventMap['assistant/chunk'],
        )
        this.#chunkSeqs.push(event.seq)
        return
      }
      case 'assistant/message':
        this.#session.append(
          'assistant/message',
          append.data as unknown as SessionEventMap['assistant/message'],
          {
            surfaceOp: 'append',
            ...(this.#chunkSeqs.length === 0
              ? {}
              : { sourceEventSeqs: [...this.#chunkSeqs] }),
          },
        )
        this.#chunkSeqs.length = 0
        return
      case 'tool/call':
        this.#session.append('tool/call', append.data as SessionEventMap['tool/call'])
        return
      case 'tool/result':
        this.#session.append(
          'tool/result',
          append.data as unknown as SessionEventMap['tool/result'],
          { surfaceOp: 'append' },
        )
    }
  }
}

function repairProjection(
  session: Session,
  projection: DshRuntimeProjection,
  runtimeEvents: readonly RuntimeEvent[],
): void {
  const expected = projection.replay(runtimeEvents)
  const actual = session.events.flatMap(event => (
    PROJECTION_EVENT_TYPES.has(event.type)
      ? [projectionKey({
        type: event.type as DshSessionAppend['type'],
        data: event.data as Readonly<Record<string, unknown>>,
      })]
      : []
  ))
  if (actual.length > expected.length) {
    throw new WinWinCodeAgentFactoryError(
      'PROJECTION_HISTORY_DIVERGED',
      `DSH session ${session.id} has more WinWinCode projection rows than its runtime ledger`,
    )
  }
  for (const [index, key] of actual.entries()) {
    const expectedAppend = expected[index]
    if (expectedAppend === undefined || key !== projectionKey(expectedAppend)) {
      throw new WinWinCodeAgentFactoryError(
        'PROJECTION_HISTORY_DIVERGED',
        `DSH session ${session.id} projection history diverged at row ${index + 1}`,
      )
    }
  }
  const writer = new DshSessionWriter(session)
  for (const append of expected.slice(actual.length)) writer.append(append)
}

function currentTurnNumber(session: Session): number {
  return session.events.findLast(event => event.type === 'turn/start')?.data.turn ?? 0
}

function approvalToolName(event: RuntimeEvent): string {
  if (event.data.type === 'apply_patch_approval_request') return 'apply_patch'
  if (event.data.type === 'exec_approval_request') return 'exec_command'
  const item = isRecord(event.data.item) ? event.data.item : undefined
  const name = event.data.tool_name ?? event.data.name ?? item?.name
  return typeof name === 'string' && name.length > 0 ? name : 'codex_operation'
}

function approvalReason(event: RuntimeEvent): string | undefined {
  const reason = event.data.reason
  return typeof reason === 'string' && reason.length > 0 ? reason : undefined
}

function approvalDecision(outcome: ApprovalOutcome): ApprovalResponse['decision'] {
  switch (outcome) {
    case 'allowed-once': return Object.freeze({ kind: 'approved' })
    case 'cancelled': return Object.freeze({ kind: 'abort' })
    case 'rejected':
      return Object.freeze({ kind: 'denied', rejection: 'The user rejected this operation.' })
    case 'unavailable':
      return Object.freeze({ kind: 'denied', rejection: 'No approval answerer was available.' })
  }
}

interface EmbeddedCodexAgentOptions {
  readonly runtimeCtx: Context
  readonly kernel: EmbeddedKernelPort
  readonly ledger: RuntimeSessionLedger
  readonly projection: DshRuntimeProjection
  readonly session: Session
  readonly options: AgentOptions
  readonly roleId: string
  readonly native: NativeRuntimeState
}

/** DSH Agent facade whose only execution authority is the embedded Codex kernel. */
export class EmbeddedCodexAgent implements Agent {
  readonly id: SessionId
  readonly options: AgentOptions
  readonly session: Session
  readonly inbox: Inbox
  readonly scope: Scope
  readonly ctx: Context

  readonly #runtimeCtx: Context
  readonly #kernel: EmbeddedKernelPort
  readonly #ledger: RuntimeSessionLedger
  readonly #projection: DshRuntimeProjection
  readonly #writer: DshSessionWriter
  readonly #dispatch: ReturnType<typeof agentEvents>
  readonly #approvalRouter: RuntimeApprovalRouter
  readonly #roleId: string
  readonly #closingStreams = new Set<string>()

  #native: NativeRuntimeState
  #status: AgentStatus = 'idle'
  #disposed = false
  #started = false
  #activity: Promise<void> = Promise.resolve()
  #pump: Promise<void> | undefined
  #activeAbort: AbortController | undefined
  #activeTurnId: string | undefined
  #turnSettlement: Deferred<void> | undefined
  #pendingHeader: EpochHeader | undefined
  #requestHeaderLogged = false
  #wakingNextStep = false
  #steering: Promise<void> = Promise.resolve()

  constructor(options: EmbeddedCodexAgentOptions) {
    this.#runtimeCtx = options.runtimeCtx
    this.#kernel = options.kernel
    this.#ledger = options.ledger
    this.#projection = options.projection
    this.#writer = new DshSessionWriter(options.session)
    this.#roleId = options.roleId
    this.#native = options.native
    this.id = options.session.id
    this.options = Object.freeze(structuredClone(options.options))
    this.session = options.session
    this.#dispatch = agentEvents(options.runtimeCtx, this)
    this.inbox = new Inbox(this.session, {
      inserted: message => { this.#dispatch.emit('agent/inbox/inserted', { message }) },
      discarded: message => { this.#dispatch.emit('agent/inbox/discarded', { message }) },
      claimed: (message, turn) => {
        this.#dispatch.emit('agent/inbox/claimed', { message, turn })
      },
    })
    this.scope = createScope(options.runtimeCtx, this)
    this.ctx = this.scope.ctx.extend({ agent: this })
    this.#approvalRouter = new RuntimeApprovalRouter(this.#kernel, this.#projection)
  }

  get status(): AgentStatus {
    return this.#status
  }

  start(): void {
    if (this.#started) throw new Error(`agent ${this.id} was already started`)
    if (this.#disposed) throw new Error(`agent ${this.id} was disposed before startup`)
    this.#started = true
    this.#startPump()
  }

  send(message: UserMessage, target: InboxTarget, wakeup: boolean): void {
    if (this.#disposed) throw new Error(`agent ${this.id} is disposed`)
    this.inbox.append(target, message)
    if (!wakeup) return
    if (target === 'next-step') {
      this.#wakingNextStep = true
      if (this.#status === 'running') {
        this.#queueSteering()
        return
      }
    }
    this.#wake()
  }

  followup(message: UserMessage): void {
    this.send(message, 'next-turn', true)
  }

  steer(message: UserMessage): void {
    this.send(message, 'next-step', true)
  }

  inject(message: UserMessage): void {
    this.send(message, 'next-step', false)
  }

  cancel(cause: AgentCancelCause, options: CancelOptions = {}): void {
    if (!options.keepInbox) {
      this.inbox.clear()
      this.#wakingNextStep = false
    }
    this.#activeAbort?.abort(cause)
    if (this.#status === 'running') {
      void this.#kernel.interrupt(this.#native.kernelSessionId).catch((error: unknown) => {
        this.#reportError(error)
      })
    }
  }

  whenIdle(): Promise<void> {
    return this.#activity
  }

  runMaintenance<T>(task: (signal: AbortSignal) => Promise<T>): Promise<T> {
    if (this.#status !== 'idle' || this.#disposed) {
      throw new Error(`agent ${this.id} is not available for maintenance`)
    }
    const abort = new AbortController()
    this.#activeAbort = abort
    const result = task(abort.signal)
    this.#activity = result.then(
      () => undefined,
      (error: unknown) => { this.#reportError(error) },
    ).finally(() => {
      if (this.#activeAbort === abort) this.#activeAbort = undefined
    })
    return result
  }

  async disposeRuntime(): Promise<void> {
    if (this.#disposed) {
      await this.#activity
      await this.#pump
      return
    }
    this.#disposed = true
    this.#activeAbort?.abort({ kind: 'disposed' } satisfies AgentCancelCause)
    this.#closingStreams.add(this.#native.kernelStreamId)
    try {
      if (this.#status === 'running') {
        await this.#kernel.interrupt(this.#native.kernelSessionId).catch(() => undefined)
      }
      await this.#kernel.closeSession(this.#native.kernelSessionId)
    } finally {
      this.#turnSettlement?.reject(new Error(`agent ${this.id} was disposed`))
      await this.#activity
      await this.#pump
    }
  }

  #setStatus(status: AgentStatus): void {
    if (this.#status === status) return
    this.#status = status
    this.#dispatch.emit('agent/status', { status })
  }

  #wake(): void {
    if (this.#status === 'running' || this.#disposed) return
    this.#setStatus('running')
    const activity = this.#drive().catch((error: unknown) => {
      this.#reportError(error)
    }).finally(() => {
      this.#activeAbort = undefined
      this.#activeTurnId = undefined
      this.#turnSettlement = undefined
      this.#setStatus('idle')
      if (!this.#disposed
        && (this.inbox.nextTurn.length > 0 || this.#wakingNextStep)) this.#wake()
    })
    this.#activity = activity
  }

  async #drive(): Promise<void> {
    while (!this.#disposed
      && (this.inbox.nextTurn.length > 0 || this.#wakingNextStep)) {
      const turn = currentTurnNumber(this.session) + 1
      const claimed = this.inbox.claim('next-turn', turn)
      this.#wakingNextStep = false
      if (claimed.length === 0) return
      const abort = new AbortController()
      this.#activeAbort = abort
      const route = await this.#resolveRoute(turn, abort.signal)
      abort.signal.throwIfAborted()
      await this.#switchRoute(route)
      this.#pendingHeader = canonicalHeader({ config: route.config })
      const settlement = deferred<void>()
      this.#turnSettlement = settlement
      const submission = await this.#kernel.submitTurn(
        this.#native.kernelSessionId,
        textFromMessages(claimed),
      )
      if (submission.status !== 'started' || submission.turnId === undefined) {
        throw new WinWinCodeAgentFactoryError(
          'KERNEL_TURN_NOT_STARTED',
          `embedded Codex did not start DSH session ${this.id}: ${submission.reason ?? submission.status}`,
        )
      }
      this.#activeTurnId = submission.turnId
      this.#queueSteering()
      await settlement.promise
      abort.signal.throwIfAborted()
      this.#activeTurnId = undefined
      this.#turnSettlement = undefined
    }
  }

  async #resolveRoute(turn: number, signal: AbortSignal): Promise<ModelRoute> {
    await this.#runtimeCtx.systemPrompt.assemble(assembleContextFor(this, signal))
    signal.throwIfAborted()
    const seed = initialRoute(this.options).config
    const config = await this.#dispatch.waterfall(
      'agent/request',
      { turn, step: 1, signal },
      () => Promise.resolve(seed),
    )
    signal.throwIfAborted()
    const provider = nonEmpty(config.provider, 'provider')
    const model = nonEmpty(config.model, 'model')
    return Object.freeze({
      config: Object.freeze(structuredClone(config)),
      provider,
      model,
    })
  }

  async #switchRoute(route: ModelRoute): Promise<void> {
    if (route.provider === this.#native.provider && route.model === this.#native.model) return
    const oldStream = this.#native.kernelStreamId
    this.#closingStreams.add(oldStream)
    await this.#kernel.closeSession(this.#native.kernelSessionId)
    await this.#pump
    const rolePolicy = roleSessionPolicy(this.#roleId)
    const resumed = await this.#kernel.resumeSession({
      rolloutPath: this.#native.rolloutPath,
      cwd: sessionCwd(this.session),
      provider: route.provider,
      model: route.model,
      ...(rolePolicy === undefined ? {} : { rolePolicy }),
    })
    const lifecycle: RuntimeKernelLifecycle = {
      kernelSessionId: resumed.sessionId,
      kernelStreamId: randomUUID(),
      rolloutPath: rolloutPath(resumed),
      provider: route.provider,
      model: route.model,
    }
    await this.#ledger.appendLifecycle(lifecycle)
    this.#native = Object.freeze({
      ...lifecycle,
      normalizedSequence: this.#projection.snapshot.asOfSequence,
    })
    this.#startPump()
  }

  #startPump(): void {
    const native = this.#native
    const projector = new CodexRuntimeProjector({
      sessionId: this.id,
      kernelSessionId: native.kernelSessionId,
      roleId: this.#roleId,
      kernelStreamId: native.kernelStreamId,
      startAfterSequence: this.#projection.snapshot.asOfSequence,
    })
    const pump = this.#pumpStream(native, projector)
    this.#pump = pump
    void pump.catch((error: unknown) => {
      this.#reportError(error)
      this.#turnSettlement?.reject(error)
    })
  }

  async #pumpStream(
    native: NativeRuntimeState,
    projector: CodexRuntimeProjector,
  ): Promise<void> {
    for await (const raw of this.#kernel.events(native.kernelSessionId)) {
      const event = projector.ingest(raw)
      if (event === undefined) continue
      await this.#ledger.appendEvent(event)
      const delta = this.#projection.apply(event)
      for (const append of delta.sessionAppends) this.#writer.append(append)
      if (event.kind === 'turn.started') this.#appendRequestHeader()
      if (event.kind === 'approval.requested') await this.#requestApproval(event)
      if ((event.kind === 'turn.completed' || event.kind === 'turn.aborted')
        && (this.#activeTurnId === undefined || event.source.turnId === this.#activeTurnId)) {
        this.#turnSettlement?.resolve()
      }
    }
    const expectedClose = this.#closingStreams.delete(native.kernelStreamId)
    if (!this.#disposed && !expectedClose) {
      throw new Error(`embedded Codex event stream ${native.kernelStreamId} closed unexpectedly`)
    }
    this.#native = Object.freeze({
      ...this.#native,
      normalizedSequence: projector.cursor.sequence,
    })
  }

  #appendRequestHeader(): void {
    const header = this.#pendingHeader
    if (header === undefined) return
    const baseline = this.session.requestHeader()
    if (!this.#requestHeaderLogged) {
      this.session.append('request/header', {
        header,
        reason: baseline === undefined ? 'initial' : 'resume',
      })
      this.#requestHeaderLogged = true
    } else if (baseline === undefined || !headerEquals(baseline, header)) {
      this.session.append('request/header', { header, reason: 'change' })
    }
    const previous = this.session.requestContext()
    if (previous?.provider !== header.config.provider || previous.model !== header.config.model) {
      this.session.append('request/context', {
        provider: header.config.provider,
        model: header.config.model,
      })
    }
    this.#pendingHeader = undefined
  }

  async #requestApproval(event: RuntimeEvent): Promise<void> {
    const approvalId = event.source.approvalId ?? event.id
    const callId = event.source.toolCallId
    const signal = this.#activeAbort?.signal
    const reason = approvalReason(event)
    const outcome = await this.#runtimeCtx.approval.request({
      agent: this,
      toolName: approvalToolName(event),
      ...(callId === undefined ? {} : { callId: CallId(callId) }),
      ...(reason === undefined ? {} : { reason }),
      ...(signal === undefined ? {} : { signal }),
    })
    await this.#approvalRouter.resolve({
      approvalId,
      decision: approvalDecision(outcome),
    })
  }

  #queueSteering(): void {
    this.#steering = this.#steering.then(async () => {
      const activeTurnId = this.#activeTurnId
      if (activeTurnId === undefined || this.inbox.nextStep.length === 0 || this.#disposed) return
      const messages = this.inbox.claim('next-step', currentTurnNumber(this.session))
      if (messages.length === 0) return
      const result = await this.#kernel.steer({
        sessionId: this.#native.kernelSessionId,
        expectedTurnId: activeTurnId,
        text: textFromMessages(messages),
      })
      if (result.status === 'steered') {
        this.#wakingNextStep = false
        return
      }
      for (const message of messages) this.inbox.append('next-turn', message)
    }).catch((error: unknown) => {
      this.#reportError(error)
    })
  }

  #reportError(error: unknown): void {
    this.#dispatch.emit('agent/error', {
      turn: currentTurnNumber(this.session),
      step: 1,
      error,
    })
  }
}

interface LifecycleResources {
  readonly agent: EmbeddedCodexAgent
  readonly ledger: RuntimeSessionLedger
  readonly freshLedger: boolean
}

/** Canonical Cordis AgentFactory replacing the stock DSH execution loop. */
export class WinWinCodeAgentFactory extends Service implements AgentFactory {
  static inject = ['agents', 'sessions', 'llm', 'systemPrompt', 'approval']

  static Config = z.object({
    home: z.string().required(),
    roleId: z.string().default(DEFAULT_ROLE_ID),
    nativeDirectory: z.string(),
    eventCapacity: z.number().step(1).min(1).max(65_536),
    shutdownTimeoutMillis: z.number().step(1).min(1).max(Number.MAX_SAFE_INTEGER),
  }) as z<Config>

  readonly config: ResolvedConfig
  private readonly kernel: EmbeddedKernelPort
  private readonly live = new Set<() => Promise<void>>()
  private disposing: Promise<void> | undefined

  constructor(
    ctx: Context,
    config: Config,
    createKernel: EmbeddedKernelFactory = options => new WinWinCodeKernel(options),
  ) {
    super(ctx, 'winwincodeAgentFactory')
    if (config.home.length === 0) {
      throw new WinWinCodeAgentFactoryError(
        'INVALID_FACTORY_CONFIG',
        'WinWinCode runtime home must not be empty',
      )
    }
    const roleId = config.roleId ?? DEFAULT_ROLE_ID
    if (roleId.length === 0) {
      throw new WinWinCodeAgentFactoryError(
        'INVALID_FACTORY_CONFIG',
        'WinWinCode DSH roleId must not be empty',
      )
    }
    this.config = Object.freeze({
      home: config.home,
      roleId,
      ...(config.nativeDirectory === undefined ? {} : { nativeDirectory: config.nativeDirectory }),
      ...(config.eventCapacity === undefined ? {} : { eventCapacity: config.eventCapacity }),
      ...(config.shutdownTimeoutMillis === undefined
        ? {}
        : { shutdownTimeoutMillis: config.shutdownTimeoutMillis }),
    })
    this.kernel = createKernel({
      home: this.config.home,
      modelPort: new DshModelPort(ctx.llm as unknown as DshLlmRuntime),
      ...(this.config.nativeDirectory === undefined
        ? {}
        : { nativeDirectory: this.config.nativeDirectory }),
      ...(this.config.eventCapacity === undefined
        ? {}
        : { eventCapacity: this.config.eventCapacity }),
      ...(this.config.shutdownTimeoutMillis === undefined
        ? {}
        : { shutdownTimeoutMillis: this.config.shutdownTimeoutMillis }),
    })
    ctx.effect(() => () => this.disposeFactory(), 'winwincodeAgentFactory.runtime()')
    ctx.effect(() => ctx.agents.setFactory(this), 'winwincodeAgentFactory.setFactory()')
  }

  /** Read the DSH-owned append-only facts used by StrongFlow projections. */
  async readRuntimeSessionEvents(dshSessionId: string): Promise<readonly RuntimeEvent[]> {
    return (await RuntimeSessionLedger.open(this.config.home, dshSessionId).then(
      ledger => ledger.read(),
    )).events
  }

  /** Read the exact persisted route and native-session owner for one DSH Session. */
  async readRuntimeSessionManifest(dshSessionId: string): Promise<RuntimeSessionManifest> {
    return RuntimeSessionLedger.open(this.config.home, dshSessionId).then(
      ledger => ledger.manifest,
    )
  }

  /** Rebuild one Delivery and select its one legal next action without executing it. */
  async reconcileDelivery(
    deliveryId: string,
    candidate: FrozenDeliveryCandidate | null = null,
  ): Promise<DeliveryRecoverySnapshot> {
    return reconcileDeliveryAfterRestart({
      home: this.config.home,
      deliveryId,
      codex: { listSessions: () => this.kernel.listSessions() },
      candidate,
    })
  }

  async createAgent(ownerCtx: Context, options: CreateAgentOptions): Promise<AgentHandle> {
    const session = this.ctx.sessions.prepare(options.sessionId, {
      ...(options.seed === undefined ? {} : { seed: options.seed }),
      ...(options.meta === undefined ? {} : { meta: options.meta }),
    })
    const roleId = sessionRoleId(session, this.config.roleId)
    const route = initialRoute(options.agentOptions ?? {})
    const rolePolicy = roleSessionPolicy(roleId)
    let info: SessionInfo
    if (options.meta?.parentSession === undefined) {
      info = await this.kernel.createSession({
        cwd: sessionCwd(session),
        provider: route.provider,
        model: route.model,
        ...(rolePolicy === undefined ? {} : { rolePolicy }),
      })
    } else {
      const parent = await RuntimeSessionLedger.open(
        this.config.home,
        options.meta.parentSession,
      )
      if (parent.manifest.roleId !== roleId) {
        throw new WinWinCodeAgentFactoryError(
          'ROLE_ID_MISMATCH',
          `DSH fork ${session.id} requests role ${roleId} but parent ${parent.manifest.dshSessionId} records ${parent.manifest.roleId}`,
        )
      }
      info = await this.kernel.forkSession({
        sourceSessionId: parent.manifest.kernelSessionId,
        cwd: sessionCwd(session),
        provider: route.provider,
        model: route.model,
      })
    }
    const native: NativeRuntimeState = Object.freeze({
      kernelSessionId: info.sessionId,
      kernelStreamId: randomUUID(),
      rolloutPath: rolloutPath(info),
      provider: route.provider,
      model: route.model,
      normalizedSequence: '0',
    })
    const ledger = await RuntimeSessionLedger.create({
      home: this.config.home,
      dshSessionId: session.id,
      roleId,
      cwd: sessionCwd(session),
      ...native,
    })
    return this.setupAndPublish(ownerCtx, options, 'startup', {
      agent: this.makeAgent(session, options.agentOptions ?? {}, ledger, native, roleId),
      ledger,
      freshLedger: true,
    })
  }

  async resume(ownerCtx: Context, options: ResumeAgentOptions): Promise<AgentHandle> {
    const persistence = this.ctx.get('sessionPersistence')
    if (persistence === undefined) {
      throw new Error('cannot resume: DSH session persistence is not configured')
    }
    const preparation: SessionPreparation = await persistence.prepare(
      options.resumeSessionId,
      options.signal,
    )
    try {
      const session = preparation.session
      const ledger = await RuntimeSessionLedger.open(this.config.home, session.id)
      const snapshot = await ledger.read()
      assertPersistedRole(session, snapshot.manifest.roleId)
      const projection = new DshRuntimeProjection({
        sessionId: session.id,
        roleId: snapshot.manifest.roleId,
        provider: snapshot.manifest.provider,
        model: snapshot.manifest.model,
      })
      repairProjection(session, projection, snapshot.events)
      const rolePolicy = roleSessionPolicy(snapshot.manifest.roleId)
      const resumed = await this.kernel.resumeSession({
        rolloutPath: snapshot.manifest.rolloutPath,
        cwd: sessionCwd(session),
        provider: snapshot.manifest.provider,
        model: snapshot.manifest.model,
        ...(rolePolicy === undefined ? {} : { rolePolicy }),
      })
      const lifecycle: RuntimeKernelLifecycle = {
        kernelSessionId: resumed.sessionId,
        kernelStreamId: randomUUID(),
        rolloutPath: rolloutPath(resumed),
        provider: snapshot.manifest.provider,
        model: snapshot.manifest.model,
      }
      await ledger.appendLifecycle(lifecycle)
      const native: NativeRuntimeState = Object.freeze({
        ...lifecycle,
        normalizedSequence: projection.snapshot.asOfSequence,
      })
      return await this.setupAndPublish(ownerCtx, options, 'resume', {
        agent: this.makeAgent(
          session,
          options.agentOptions ?? {},
          ledger,
          native,
          snapshot.manifest.roleId,
          projection,
        ),
        ledger,
        freshLedger: false,
      })
    } finally {
      preparation[Symbol.dispose]()
    }
  }

  private makeAgent(
    session: Session,
    options: AgentOptions,
    ledger: RuntimeSessionLedger,
    native: NativeRuntimeState,
    roleId: string,
    projection = new DshRuntimeProjection({
      sessionId: session.id,
      roleId,
      provider: native.provider,
      model: native.model,
    }),
  ): EmbeddedCodexAgent {
    return new EmbeddedCodexAgent({
      runtimeCtx: this.ctx,
      kernel: this.kernel,
      ledger,
      projection,
      session,
      options,
      roleId,
      native,
    })
  }

  private async setupAndPublish(
    ownerCtx: Context,
    options: Pick<CreateAgentOptions, 'setup' | 'signal'> | ResumeAgentOptions,
    source: SessionStartSource,
    resources: LifecycleResources,
  ): Promise<AgentHandle> {
    ownerCtx.fiber.assertActive()
    options.signal?.throwIfAborted()
    const { agent } = resources
    let detachSession: (() => void) | undefined
    let detachAgent: (() => void) | undefined
    let ownerDisposer: (() => Promise<void> | void) | undefined
    let published = false
    let disposing: Promise<void> | undefined
    const dispose = (): Promise<void> => (disposing ??= (async () => {
      try {
        try {
          await agent.disposeRuntime()
        } finally {
          await agent.scope.dispose()
        }
      } finally {
        detachAgent?.()
        detachSession?.()
        this.live.delete(dispose)
      }
    })())
    this.live.add(dispose)
    try {
      ownerDisposer = ownerCtx.effect(
        () => () => dispose(),
        `winwincodeAgentFactory.lifecycle(${agent.id})`,
      )
      const commit = await (options.setup as AgentSetup | undefined)?.(agent.ctx)
      options.signal?.throwIfAborted()
      ownerCtx.fiber.assertActive()
      commit?.commit()
      detachSession = agent.ctx.sessions.enter(agent.session)
      detachAgent = this.ctx.agents.enter(agent, ownerCtx.agent)
      agent.ctx.sessions.announce(agent.session)
      this.ctx.agents.announce(agent)
      emitAgentEvent(this.ctx, agent, 'agent/session-start', { source })
      agent.start()
      published = true
      return {
        agent,
        async dispose(): Promise<void> {
          await ownerDisposer?.()
          await dispose()
        },
      }
    } catch (error) {
      await dispose()
      if (resources.freshLedger && !published) await resources.ledger.discard()
      throw error
    }
  }

  private disposeFactory(): Promise<void> {
    return (this.disposing ??= (async () => {
      try {
        await Promise.all([...this.live].map(dispose => dispose()))
      } finally {
        await this.kernel.shutdown()
      }
    })())
  }
}

export default WinWinCodeAgentFactory
