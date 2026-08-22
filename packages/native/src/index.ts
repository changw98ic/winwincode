import type {
  StrongFlowPermissionPresetId,
  StrongFlowRoleId,
  StrongFlowRoleTool,
  StrongFlowRoleWorkspaceMode,
  SupportedReleaseTarget,
  WorkspaceComponentDescriptor,
} from '@winwincode/contracts'
import {
  STRONGFLOW_PERMISSION_PRESET_IDS,
  STRONGFLOW_ROLE_IDS,
  STRONGFLOW_ROLE_TOOLS,
} from '@winwincode/contracts'
import { createRequire } from 'node:module'
import { dirname, isAbsolute, join, resolve } from 'node:path'

const RELEASE_TARGET_BY_HOST = new Map<string, SupportedReleaseTarget>([
  ['darwin/arm64', 'aarch64-apple-darwin'],
  ['darwin/x64', 'x86_64-apple-darwin'],
  ['linux/arm64', 'aarch64-unknown-linux-gnu'],
  ['linux/x64', 'x86_64-unknown-linux-gnu'],
])
const NATIVE_PACKAGE_BY_TARGET = new Map<SupportedReleaseTarget, string>([
  ['aarch64-apple-darwin', '@winwincode/native-darwin-arm64'],
  ['x86_64-apple-darwin', '@winwincode/native-darwin-x64'],
  ['aarch64-unknown-linux-gnu', '@winwincode/native-linux-arm64'],
  ['x86_64-unknown-linux-gnu', '@winwincode/native-linux-x64'],
])
const NATIVE_ERROR_PREFIX = 'WINWINCODE_KERNEL_ERROR'
const DEFAULT_EVENT_POLL_MILLIS = 250
const UINT32_MAX = 4_294_967_295
const loadNativeModule = createRequire(import.meta.url)

export class UnsupportedPlatformError extends Error {
  readonly platform: string
  readonly architecture: string

  constructor(platform: string, architecture: string) {
    super(
      `Unsupported platform ${platform}/${architecture}. `
      + 'WinWinCode supports macOS and Linux on arm64 or x64.',
    )
    this.name = 'UnsupportedPlatformError'
    this.platform = platform
    this.architecture = architecture
  }
}

export class KernelError extends Error {
  readonly code: string
  readonly detail: string

  constructor(code: string, detail: string, cause?: unknown) {
    super(detail, cause === undefined ? undefined : { cause })
    this.name = 'KernelError'
    this.code = code
    this.detail = detail
  }
}

export interface ModelPortRequest {
  readonly requestId: string
  readonly provider: string
  readonly sessionId: string
  readonly threadId: string
  readonly turnId?: string | null
  readonly request: unknown
}

export interface ModelPortFailure {
  readonly code: string
  readonly message: string
  readonly status?: number
  readonly providerRetryAfterMillis?: number
  readonly providerRequestId?: string
}

export interface CodexTokenUsage {
  readonly input_tokens: number
  readonly cached_input_tokens: number
  readonly cache_write_input_tokens: number
  readonly output_tokens: number
  readonly reasoning_output_tokens: number
  readonly total_tokens: number
}

export type ModelPortMessage =
  | { readonly type: 'created' }
  | { readonly type: 'server_model'; readonly model: string }
  | { readonly type: 'output_item_added'; readonly item: unknown }
  | { readonly type: 'output_item_done'; readonly item: unknown }
  | { readonly type: 'output_text_delta'; readonly delta: string }
  | {
    readonly type: 'tool_call_input_delta'
    readonly itemId: string
    readonly callId?: string
    readonly delta: string
  }
  | {
    readonly type: 'reasoning_summary_delta'
    readonly delta: string
    readonly summaryIndex: number
  }
  | {
    readonly type: 'reasoning_summary_done'
    readonly itemId: string
    readonly text: string
    readonly summaryIndex: number
  }
  | {
    readonly type: 'reasoning_content_delta'
    readonly delta: string
    readonly contentIndex: number
  }
  | { readonly type: 'reasoning_summary_part_added'; readonly summaryIndex: number }
  | {
    readonly type: 'completed'
    readonly responseId: string
    readonly tokenUsage?: CodexTokenUsage
    readonly endTurn?: boolean
  }
  | { readonly type: 'error'; readonly error: ModelPortFailure }

export interface ModelPort {
  stream(request: ModelPortRequest, signal: AbortSignal): AsyncIterable<ModelPortMessage>
}

export class ModelPortError extends Error {
  readonly failure: ModelPortFailure

  constructor(failure: ModelPortFailure, cause?: unknown) {
    super(failure.message, cause === undefined ? undefined : { cause })
    this.name = 'ModelPortError'
    this.failure = Object.freeze({ ...failure })
  }
}

export interface KernelOptions {
  readonly home: string
  readonly modelPort: ModelPort
  readonly eventCapacity?: number
  readonly shutdownTimeoutMillis?: number
  readonly nativeDirectory?: string
}

export interface SessionOptions {
  readonly cwd: string
  readonly provider: string
  readonly model: string
  readonly governedAuthority?: GovernedSessionAuthority
}

export interface ResumeOptions extends SessionOptions {
  readonly rolloutPath: string
}

export interface ForkOptions {
  readonly sourceSessionId: string
  readonly cwd?: string
  readonly provider?: string
  readonly model?: string
}

export interface SteerOptions {
  readonly sessionId: string
  readonly expectedTurnId: string
  readonly text: string
}

export type ApprovalDecision =
  | { readonly kind: 'approved' }
  | { readonly kind: 'approved_for_session' }
  | { readonly kind: 'denied'; readonly rejection: string }
  | { readonly kind: 'abort' }

export interface ApprovalResponse {
  readonly sessionId: string
  readonly kind: 'exec' | 'patch'
  readonly operationId: string
  readonly turnId?: string
  readonly decision: ApprovalDecision
}

export const GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION = 1 as const

/** Immutable StrongFlow authority that must be applied before Codex starts a thread. */
export interface GovernedSessionAuthority {
  readonly schemaVersion: typeof GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION
  readonly roleId: StrongFlowRoleId
  readonly permissionPreset: StrongFlowPermissionPresetId
  readonly workspaceMode: StrongFlowRoleWorkspaceMode
  readonly workspaceRoot: string
  readonly systemInstructions: string
  readonly reasoningEffort: string | null
  readonly visibleTools: readonly StrongFlowRoleTool[]
}

/** Actual Codex thread settings observed after startup and before host publication. */
export interface GovernedSessionEffectivePolicy {
  readonly schemaVersion: typeof GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION
  readonly authority: 'codex-core'
  readonly roleId: StrongFlowRoleId
  readonly permissionPreset: StrongFlowPermissionPresetId
  readonly workspaceMode: StrongFlowRoleWorkspaceMode
  readonly workspaceRoot: string
  readonly visibleTools: readonly StrongFlowRoleTool[]
  readonly filesystem: 'managed-read-only' | 'managed-workspace-write'
  readonly network: 'restricted'
  readonly process: 'dynamic-tools-with-governed-command-api'
  readonly environment: 'empty'
  readonly governedProcess: 'platform-sandbox-required'
  readonly governedProcessNetwork: 'restricted'
  readonly governedProcessEnvironment: 'explicit-allowlist'
  readonly credentials: 'dsh-reference-only'
  readonly approvalPolicy: 'on-request'
  readonly approvalsReviewer: 'user'
  readonly loginShell: false
  readonly environmentSelections: readonly []
  readonly instructionSources: readonly []
}

/** Text result for one suspended Codex dynamic-tool call. */
export interface DynamicToolResponse {
  readonly sessionId: string
  readonly callId: string
  readonly success: boolean
  readonly text: string
}

export const GOVERNED_COMMAND_SCHEMA_VERSION = 1 as const

export type GovernedCommandStatus =
  | 'exited'
  | 'sandbox-denied'
  | 'timed-out'
  | 'cancelled'
  | 'output-limit'

/** Trusted command grant. The native kernel derives workspace and role authority from sessionId. */
export interface GovernedCommandRequest {
  readonly schemaVersion: typeof GOVERNED_COMMAND_SCHEMA_VERSION
  readonly sessionId: string
  readonly commandId: string
  readonly tool: 'command.run' | 'test.run'
  readonly argv: readonly string[]
  readonly cwd: string
  readonly environment: Readonly<Record<string, string>>
  readonly timeoutMillis: number
  readonly outputLimitBytes: number
}

/** Bounded output plus the concrete host enforcement used for one governed command. */
export interface GovernedCommandResult {
  readonly schemaVersion: typeof GOVERNED_COMMAND_SCHEMA_VERSION
  readonly sessionId: string
  readonly commandId: string
  readonly status: GovernedCommandStatus
  readonly exitCode?: number
  readonly stdout: string
  readonly stderr: string
  readonly sandbox: 'macos-seatbelt' | 'linux-seccomp'
  readonly network: 'restricted'
  readonly environmentNames: readonly string[]
}

export interface KernelBuildInfo {
  readonly interfaceVersion: number
  readonly codexTag: string
  readonly codexCommit: string
  readonly patchSet: readonly string[]
  readonly eventCapacity: number
}

export interface SessionInfo {
  readonly sessionId: string
  readonly rolloutPath?: string
  readonly effectivePolicy?: GovernedSessionEffectivePolicy
}

export type SubmissionStatus = 'started' | 'steered' | 'not_submitted'

export interface SubmissionInfo {
  readonly status: SubmissionStatus
  readonly turnId?: string
  readonly reason?: string
}

export interface ShutdownInfo {
  readonly completed: readonly string[]
  readonly submitFailed: readonly string[]
  readonly timedOut: readonly string[]
}

export interface KernelEvent {
  readonly sequence: bigint
  readonly kind: string
  readonly payload: unknown
  readonly rawJson: string
}

export type EventPoll =
  | { readonly status: 'event'; readonly event: KernelEvent }
  | { readonly status: 'timeout' }
  | { readonly status: 'closed' }

export interface EventStreamOptions {
  readonly signal?: AbortSignal
  readonly timeoutMillis?: number
}

interface NativeKernelConstructionOptions {
  home: string
  helperExecutable: string
  eventCapacity?: number
  shutdownTimeoutMillis?: number
  linuxSandboxExecutable?: string
}

interface NativeSessionOptions {
  cwd: string
  provider: string
  model: string
  governedAuthorityJson?: string
}

interface NativeResumeOptions extends NativeSessionOptions {
  rolloutPath: string
}

interface NativeForkOptions {
  sourceSessionId: string
  cwd?: string
  provider?: string
  model?: string
}

interface NativeSteerOptions {
  sessionId: string
  expectedTurnId: string
  text: string
}

interface NativeApprovalResponse {
  sessionId: string
  kind: string
  operationId: string
  turnId?: string
  decision: string
  rejection?: string
}

interface NativeDynamicToolResponse {
  sessionId: string
  callId: string
  success: boolean
  text: string
}

interface NativeGovernedCommandRequest {
  schemaVersion: number
  sessionId: string
  commandId: string
  tool: string
  argv: string[]
  cwd: string
  environmentJson: string
  timeoutMillis: number
  outputLimitBytes: number
}

interface NativeGovernedCommandResult {
  schemaVersion: number
  sessionId: string
  commandId: string
  status: string
  exitCode?: number | null
  stdout: string
  stderr: string
  sandbox: string
  network: string
  environmentNames: string[]
}

interface NativeBuildInfo {
  interfaceVersion: number
  codexTag: string
  codexCommit: string
  patchSet: string[]
  eventCapacity: number
}

interface NativeSessionInfo {
  sessionId: string
  rolloutPath?: string | null
  effectivePolicyJson?: string | null
}

interface NativeSubmissionInfo {
  status: string
  turnId?: string | null
  reason?: string | null
}

interface NativeShutdownInfo {
  completed: string[]
  submitFailed: string[]
  timedOut: string[]
}

interface NativeEvent {
  sequence: string
  kind: string
  payloadJson: string
}

interface NativeEventPoll {
  status: string
  event?: NativeEvent | null
}

interface NativeKernelBinding {
  buildInfo(): NativeBuildInfo
  createSession(options: NativeSessionOptions): Promise<NativeSessionInfo>
  resumeSession(options: NativeResumeOptions): Promise<NativeSessionInfo>
  forkSession(options: NativeForkOptions): Promise<NativeSessionInfo>
  submitTurn(sessionId: string, text: string): Promise<NativeSubmissionInfo>
  steer(options: NativeSteerOptions): Promise<NativeSubmissionInfo>
  interrupt(sessionId: string): Promise<string>
  resolveApproval(response: NativeApprovalResponse): Promise<string>
  resolveDynamicTool(response: NativeDynamicToolResponse): Promise<string>
  executeGovernedCommand(
    request: NativeGovernedCommandRequest,
  ): Promise<NativeGovernedCommandResult>
  cancelGovernedCommand(sessionId: string, commandId: string): Promise<void>
  nextEvent(sessionId: string, timeoutMillis?: number): Promise<NativeEventPoll>
  listSessions(): Promise<string[]>
  closeSession(sessionId: string): Promise<void>
  shutdown(): Promise<NativeShutdownInfo>
}

interface NativeKernelConstructor {
  new(
    options: NativeKernelConstructionOptions,
    modelStream: (payloadJson: string) => ReadableStream<string>,
    modelCancel: (requestId: string) => void,
  ): NativeKernelBinding
  readonly prototype: NativeKernelBinding
}

interface NativeBindingModule {
  readonly NativeKernel: NativeKernelConstructor
}

export function resolveReleaseTarget(
  platform: string = process.platform,
  architecture: string = process.arch,
): SupportedReleaseTarget {
  const target = RELEASE_TARGET_BY_HOST.get(`${platform}/${architecture}`)
  if (target === undefined) throw new UnsupportedPlatformError(platform, architecture)
  return target
}

export function nativePackageName(target: SupportedReleaseTarget): string {
  const packageName = NATIVE_PACKAGE_BY_TARGET.get(target)
  if (packageName === undefined) {
    throw new KernelError('NATIVE_TARGET_INVALID', `unknown release target ${target}`)
  }
  return packageName
}

function defaultNativeDirectory(target: SupportedReleaseTarget): string {
  const packageName = nativePackageName(target)
  try {
    return dirname(loadNativeModule.resolve(`${packageName}/build-info.json`))
  } catch (error) {
    throw new KernelError(
      'NATIVE_PACKAGE_MISSING',
      `native package ${packageName} is not installed for ${target}`,
      error,
    )
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null
}

function structuredModelPortFailure(error: unknown): ModelPortFailure {
  if (error instanceof ModelPortError) return error.failure
  return Object.freeze({
    code: 'MODEL_RUNTIME_FAILED',
    message: 'DSH model runtime failed without a structured failure',
  })
}

function parseModelPortRequest(payloadJson: string): ModelPortRequest {
  let value: unknown
  try {
    value = JSON.parse(payloadJson) as unknown
  } catch (error) {
    throw new ModelPortError({
      code: 'MODEL_PORT_REQUEST_INVALID',
      message: 'native kernel supplied invalid model request JSON',
    }, error)
  }
  if (
    !isRecord(value)
    || typeof value.requestId !== 'string'
    || value.requestId.length === 0
    || typeof value.provider !== 'string'
    || value.provider.length === 0
    || typeof value.sessionId !== 'string'
    || value.sessionId.length === 0
    || typeof value.threadId !== 'string'
    || value.threadId.length === 0
    || !('request' in value)
    || (
      value.turnId !== undefined
      && value.turnId !== null
      && typeof value.turnId !== 'string'
    )
  ) {
    throw new ModelPortError({
      code: 'MODEL_PORT_REQUEST_INVALID',
      message: 'native kernel supplied an invalid model request envelope',
    })
  }
  return value as unknown as ModelPortRequest
}

function serializeModelPortMessage(message: ModelPortMessage): string {
  if (!isRecord(message) || typeof message.type !== 'string') {
    throw new ModelPortError({
      code: 'MODEL_PORT_PROTOCOL_INVALID',
      message: 'model port returned a message without a type',
    })
  }
  let payload: string | undefined
  try {
    payload = JSON.stringify(message)
  } catch (error) {
    throw new ModelPortError({
      code: 'MODEL_PORT_PROTOCOL_INVALID',
      message: 'model port returned a message that is not JSON serializable',
    }, error)
  }
  if (payload === undefined) {
    throw new ModelPortError({
      code: 'MODEL_PORT_PROTOCOL_INVALID',
      message: 'model port returned an empty serialized message',
    })
  }
  return payload
}

function errorMessage(failure: ModelPortFailure): string {
  return serializeModelPortMessage({ type: 'error', error: failure })
}

function loadBinding(path: string): NativeBindingModule {
  let loaded: unknown
  try {
    loaded = loadNativeModule(path)
  } catch (error) {
    throw new KernelError('NATIVE_LOAD_FAILED', `failed to load native kernel at ${path}`, error)
  }
  if (!isRecord(loaded) || typeof loaded.NativeKernel !== 'function') {
    throw new KernelError(
      'NATIVE_PROTOCOL_INVALID',
      `native kernel at ${path} does not export NativeKernel`,
    )
  }
  const constructor = loaded.NativeKernel as NativeKernelConstructor
  const requiredMethods: readonly (keyof NativeKernelBinding)[] = [
    'buildInfo',
    'createSession',
    'resumeSession',
    'forkSession',
    'submitTurn',
    'steer',
    'interrupt',
    'resolveApproval',
    'resolveDynamicTool',
    'executeGovernedCommand',
    'cancelGovernedCommand',
    'nextEvent',
    'listSessions',
    'closeSession',
    'shutdown',
  ]
  for (const method of requiredMethods) {
    if (typeof constructor.prototype[method] !== 'function') {
      throw new KernelError(
        'NATIVE_PROTOCOL_INVALID',
        `native kernel at ${path} is missing method ${method}`,
      )
    }
  }
  return { NativeKernel: constructor }
}

function translateError(error: unknown, fallbackCode = 'NATIVE_CALL_FAILED'): KernelError {
  if (error instanceof KernelError) return error
  const message = error instanceof Error ? error.message : String(error)
  const marker = `${NATIVE_ERROR_PREFIX}|`
  const markerIndex = message.indexOf(marker)
  if (markerIndex >= 0) {
    const envelope = message.slice(markerIndex + marker.length)
    const separator = envelope.indexOf('|')
    if (separator > 0) {
      return new KernelError(
        envelope.slice(0, separator),
        envelope.slice(separator + 1),
        error,
      )
    }
  }
  return new KernelError(fallbackCode, message, error)
}

function validateUint32(value: number, label: string): void {
  if (!Number.isInteger(value) || value < 0 || value > UINT32_MAX) {
    throw new KernelError('INVALID_ARGUMENT', `${label} must be an unsigned 32-bit integer`)
  }
}

function optionalString(value: string | null | undefined): string | undefined {
  return value === null ? undefined : value
}

function exactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[],
  label: string,
): void {
  const allowed = new Set([...required, ...optional])
  if (
    required.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !allowed.has(key))
  ) throw new KernelError('INVALID_ARGUMENT', `${label} has an unexpected shape`)
}

function governedAuthorityJson(
  authority: GovernedSessionAuthority,
  cwd: string,
): string {
  if (!isRecord(authority) || Array.isArray(authority)) {
    throw new KernelError('INVALID_ARGUMENT', 'governed session authority must be an object')
  }
  exactKeys(authority, [
    'schemaVersion',
    'roleId',
    'permissionPreset',
    'workspaceMode',
    'workspaceRoot',
    'systemInstructions',
    'reasoningEffort',
    'visibleTools',
  ], [], 'governed session authority')
  if (
    authority.schemaVersion !== GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION
    || !STRONGFLOW_ROLE_IDS.includes(authority.roleId)
    || !STRONGFLOW_PERMISSION_PRESET_IDS.includes(authority.permissionPreset)
    || !['source-read-only', 'candidate-read-only', 'candidate-write'].includes(
      authority.workspaceMode,
    )
    || typeof authority.workspaceRoot !== 'string'
    || !isAbsolute(authority.workspaceRoot)
    || resolve(authority.workspaceRoot) !== resolve(cwd)
    || typeof authority.systemInstructions !== 'string'
    || authority.systemInstructions.trim().length === 0
    || (
      authority.reasoningEffort !== null
      && (
        typeof authority.reasoningEffort !== 'string'
        || authority.reasoningEffort.trim().length === 0
      )
    )
    || !Array.isArray(authority.visibleTools)
    || new Set(authority.visibleTools).size !== authority.visibleTools.length
    || authority.visibleTools.some(tool => !STRONGFLOW_ROLE_TOOLS.includes(tool))
  ) throw new KernelError('INVALID_ARGUMENT', 'governed session authority is invalid')
  return JSON.stringify(authority)
}

function effectivePolicy(value: string): GovernedSessionEffectivePolicy {
  let parsed: unknown
  try {
    parsed = JSON.parse(value) as unknown
  } catch (error) {
    throw new KernelError(
      'NATIVE_PROTOCOL_INVALID',
      'native kernel returned invalid effective-policy JSON',
      error,
    )
  }
  if (!isRecord(parsed) || Array.isArray(parsed)) {
    throw new KernelError('NATIVE_PROTOCOL_INVALID', 'native effective policy is not an object')
  }
  try {
    exactKeys(parsed, [
      'schemaVersion',
      'authority',
      'roleId',
      'permissionPreset',
      'workspaceMode',
      'workspaceRoot',
      'visibleTools',
      'filesystem',
      'network',
      'process',
      'environment',
      'governedProcess',
      'governedProcessNetwork',
      'governedProcessEnvironment',
      'credentials',
      'approvalPolicy',
      'approvalsReviewer',
      'loginShell',
      'environmentSelections',
      'instructionSources',
    ], [], 'native effective policy')
  } catch (error) {
    throw new KernelError(
      'NATIVE_PROTOCOL_INVALID',
      'native effective policy has an unexpected shape',
      error,
    )
  }
  if (
    parsed.schemaVersion !== GOVERNED_SESSION_AUTHORITY_SCHEMA_VERSION
    || parsed.authority !== 'codex-core'
    || typeof parsed.roleId !== 'string'
    || !STRONGFLOW_ROLE_IDS.includes(parsed.roleId as StrongFlowRoleId)
    || typeof parsed.permissionPreset !== 'string'
    || !STRONGFLOW_PERMISSION_PRESET_IDS.includes(
      parsed.permissionPreset as StrongFlowPermissionPresetId,
    )
    || !['source-read-only', 'candidate-read-only', 'candidate-write'].includes(
      String(parsed.workspaceMode),
    )
    || typeof parsed.workspaceRoot !== 'string'
    || !isAbsolute(parsed.workspaceRoot)
    || !Array.isArray(parsed.visibleTools)
    || new Set(parsed.visibleTools).size !== parsed.visibleTools.length
    || parsed.visibleTools.some(tool => (
      typeof tool !== 'string'
      || !STRONGFLOW_ROLE_TOOLS.includes(tool as StrongFlowRoleTool)
    ))
    || !['managed-read-only', 'managed-workspace-write'].includes(String(parsed.filesystem))
    || parsed.network !== 'restricted'
    || parsed.process !== 'dynamic-tools-with-governed-command-api'
    || parsed.environment !== 'empty'
    || parsed.governedProcess !== 'platform-sandbox-required'
    || parsed.governedProcessNetwork !== 'restricted'
    || parsed.governedProcessEnvironment !== 'explicit-allowlist'
    || parsed.credentials !== 'dsh-reference-only'
    || parsed.approvalPolicy !== 'on-request'
    || parsed.approvalsReviewer !== 'user'
    || parsed.loginShell !== false
    || !Array.isArray(parsed.environmentSelections)
    || parsed.environmentSelections.length !== 0
    || !Array.isArray(parsed.instructionSources)
    || parsed.instructionSources.length !== 0
  ) throw new KernelError('NATIVE_PROTOCOL_INVALID', 'native effective policy is invalid')
  return Object.freeze({
    ...parsed,
    visibleTools: Object.freeze([...parsed.visibleTools] as StrongFlowRoleTool[]),
    environmentSelections: Object.freeze([]),
    instructionSources: Object.freeze([]),
  }) as GovernedSessionEffectivePolicy
}

function sessionInfo(info: NativeSessionInfo): SessionInfo {
  const rolloutPath = optionalString(info.rolloutPath)
  const effectivePolicyJson = optionalString(info.effectivePolicyJson)
  return Object.freeze({
    sessionId: info.sessionId,
    ...(rolloutPath === undefined ? {} : { rolloutPath }),
    ...(effectivePolicyJson === undefined
      ? {}
      : { effectivePolicy: effectivePolicy(effectivePolicyJson) }),
  })
}

function submissionInfo(info: NativeSubmissionInfo): SubmissionInfo {
  if (!['started', 'steered', 'not_submitted'].includes(info.status)) {
    throw new KernelError(
      'NATIVE_PROTOCOL_INVALID',
      `native kernel returned unknown submission status ${info.status}`,
    )
  }
  const result: {
    status: SubmissionStatus
    turnId?: string
    reason?: string
  } = { status: info.status as SubmissionStatus }
  const turnId = optionalString(info.turnId)
  const reason = optionalString(info.reason)
  if (turnId !== undefined) result.turnId = turnId
  if (reason !== undefined) result.reason = reason
  return result
}

function eventInfo(event: NativeEvent): KernelEvent {
  if (!/^\d+$/u.test(event.sequence)) {
    throw new KernelError(
      'NATIVE_PROTOCOL_INVALID',
      `native kernel returned invalid event sequence ${event.sequence}`,
    )
  }
  let payload: unknown
  try {
    payload = JSON.parse(event.payloadJson) as unknown
  } catch (error) {
    throw new KernelError(
      'INVALID_EVENT_PAYLOAD',
      `native kernel returned invalid JSON for event ${event.sequence}`,
      error,
    )
  }
  return {
    sequence: BigInt(event.sequence),
    kind: event.kind,
    payload,
    rawJson: event.payloadJson,
  }
}

function eventPoll(poll: NativeEventPoll): EventPoll {
  if (poll.status === 'event' && poll.event !== undefined && poll.event !== null) {
    return { status: 'event', event: eventInfo(poll.event) }
  }
  if (poll.status === 'timeout') return { status: 'timeout' }
  if (poll.status === 'closed') return { status: 'closed' }
  throw new KernelError(
    'NATIVE_PROTOCOL_INVALID',
    `native kernel returned invalid event poll status ${poll.status}`,
  )
}

function nativeSessionOptions(options: SessionOptions): NativeSessionOptions {
  return {
    cwd: options.cwd,
    provider: options.provider,
    model: options.model,
    ...(options.governedAuthority === undefined
      ? {}
      : { governedAuthorityJson: governedAuthorityJson(options.governedAuthority, options.cwd) }),
  }
}

function nativeDynamicToolResponse(response: DynamicToolResponse): NativeDynamicToolResponse {
  if (
    !isRecord(response)
    || Array.isArray(response)
    || typeof response.sessionId !== 'string'
    || response.sessionId.trim().length === 0
    || typeof response.callId !== 'string'
    || response.callId.trim().length === 0
    || typeof response.success !== 'boolean'
    || typeof response.text !== 'string'
  ) throw new KernelError('INVALID_DYNAMIC_TOOL_RESPONSE', 'dynamic-tool response is invalid')
  exactKeys(
    response,
    ['sessionId', 'callId', 'success', 'text'],
    [],
    'dynamic-tool response',
  )
  return { ...response }
}

function nativeGovernedCommandRequest(
  request: GovernedCommandRequest,
): NativeGovernedCommandRequest {
  if (!isRecord(request) || Array.isArray(request)) {
    throw new KernelError('INVALID_GOVERNED_COMMAND', 'governed command must be an object')
  }
  exactKeys(request, [
    'schemaVersion',
    'sessionId',
    'commandId',
    'tool',
    'argv',
    'cwd',
    'environment',
    'timeoutMillis',
    'outputLimitBytes',
  ], [], 'governed command')
  if (
    request.schemaVersion !== GOVERNED_COMMAND_SCHEMA_VERSION
    || typeof request.sessionId !== 'string'
    || request.sessionId.length === 0
    || typeof request.commandId !== 'string'
    || request.commandId.length === 0
    || !['command.run', 'test.run'].includes(request.tool)
    || !Array.isArray(request.argv)
    || request.argv.length === 0
    || request.argv.some(argument => typeof argument !== 'string' || argument.length === 0)
    || typeof request.cwd !== 'string'
    || !isAbsolute(request.cwd)
    || !isRecord(request.environment)
    || Array.isArray(request.environment)
    || Object.values(request.environment).some(value => typeof value !== 'string')
  ) throw new KernelError('INVALID_GOVERNED_COMMAND', 'governed command is invalid')
  validateUint32(request.timeoutMillis, 'governed command timeoutMillis')
  validateUint32(request.outputLimitBytes, 'governed command outputLimitBytes')
  return {
    schemaVersion: request.schemaVersion,
    sessionId: request.sessionId,
    commandId: request.commandId,
    tool: request.tool,
    argv: [...request.argv],
    cwd: request.cwd,
    environmentJson: JSON.stringify(request.environment),
    timeoutMillis: request.timeoutMillis,
    outputLimitBytes: request.outputLimitBytes,
  }
}

function governedCommandResult(result: NativeGovernedCommandResult): GovernedCommandResult {
  const statuses: readonly GovernedCommandStatus[] = [
    'exited',
    'sandbox-denied',
    'timed-out',
    'cancelled',
    'output-limit',
  ]
  if (
    result.schemaVersion !== GOVERNED_COMMAND_SCHEMA_VERSION
    || typeof result.sessionId !== 'string'
    || typeof result.commandId !== 'string'
    || !statuses.includes(result.status as GovernedCommandStatus)
    || (result.exitCode !== undefined && result.exitCode !== null
      && !Number.isInteger(result.exitCode))
    || typeof result.stdout !== 'string'
    || typeof result.stderr !== 'string'
    || !['macos-seatbelt', 'linux-seccomp'].includes(result.sandbox)
    || result.network !== 'restricted'
    || !Array.isArray(result.environmentNames)
    || result.environmentNames.some(name => typeof name !== 'string')
  ) throw new KernelError(
    'NATIVE_PROTOCOL_INVALID',
    'native kernel returned an invalid governed command result',
  )
  return Object.freeze({
    schemaVersion: GOVERNED_COMMAND_SCHEMA_VERSION,
    sessionId: result.sessionId,
    commandId: result.commandId,
    status: result.status as GovernedCommandStatus,
    ...(result.exitCode === undefined || result.exitCode === null
      ? {}
      : { exitCode: result.exitCode }),
    stdout: result.stdout,
    stderr: result.stderr,
    sandbox: result.sandbox as GovernedCommandResult['sandbox'],
    network: 'restricted',
    environmentNames: Object.freeze([...result.environmentNames]),
  })
}

function nativeApprovalResponse(response: ApprovalResponse): NativeApprovalResponse {
  if (response.sessionId.trim().length === 0 || response.operationId.trim().length === 0) {
    throw new KernelError(
      'INVALID_APPROVAL_RESPONSE',
      'approval session and operation identities must be non-empty',
    )
  }
  if (response.turnId !== undefined && response.turnId.trim().length === 0) {
    throw new KernelError('INVALID_APPROVAL_RESPONSE', 'approval turn identity must be non-empty')
  }
  if (response.decision.kind === 'denied' && response.decision.rejection.trim().length === 0) {
    throw new KernelError(
      'INVALID_APPROVAL_RESPONSE',
      'denied approval must include a rejection reason',
    )
  }
  return {
    sessionId: response.sessionId,
    kind: response.kind,
    operationId: response.operationId,
    decision: response.decision.kind,
    ...(response.turnId === undefined ? {} : { turnId: response.turnId }),
    ...(response.decision.kind === 'denied'
      ? { rejection: response.decision.rejection }
      : {}),
  }
}

interface ActiveModelOperation {
  readonly abortController: AbortController
  iterator?: AsyncIterator<ModelPortMessage>
}

export class WinWinCodeKernel {
  readonly buildInfo: KernelBuildInfo
  readonly target: SupportedReleaseTarget

  readonly #binding: NativeKernelBinding
  readonly #modelPort: ModelPort
  readonly #eventSubscribers = new Set<string>()
  readonly #modelOperations = new Map<string, ActiveModelOperation>()
  #shutdownPromise: Promise<ShutdownInfo> | undefined

  constructor(options: KernelOptions) {
    this.#modelPort = options.modelPort
    this.target = resolveReleaseTarget()
    const nativeDirectory = resolve(options.nativeDirectory ?? defaultNativeDirectory(this.target))
    const bindingPath = join(nativeDirectory, 'winwincode_native.node')
    const helperExecutable = join(nativeDirectory, 'winwincode-kernel-helper')
    const binding = loadBinding(bindingPath)
    const nativeOptions: NativeKernelConstructionOptions = {
      home: options.home,
      helperExecutable,
    }
    if (options.eventCapacity !== undefined) {
      validateUint32(options.eventCapacity, 'eventCapacity')
      nativeOptions.eventCapacity = options.eventCapacity
    }
    if (options.shutdownTimeoutMillis !== undefined) {
      validateUint32(options.shutdownTimeoutMillis, 'shutdownTimeoutMillis')
      nativeOptions.shutdownTimeoutMillis = options.shutdownTimeoutMillis
    }
    if (process.platform === 'linux') {
      nativeOptions.linuxSandboxExecutable = join(nativeDirectory, 'codex-linux-sandbox')
    }
    try {
      this.#binding = new binding.NativeKernel(
        nativeOptions,
        payloadJson => this.#openModelStream(payloadJson),
        requestId => this.#cancelModelStream(requestId),
      )
      const build = this.#binding.buildInfo()
      this.buildInfo = Object.freeze({
        interfaceVersion: build.interfaceVersion,
        codexTag: build.codexTag,
        codexCommit: build.codexCommit,
        patchSet: Object.freeze([...build.patchSet]),
        eventCapacity: build.eventCapacity,
      })
    } catch (error) {
      throw translateError(error)
    }
  }

  async createSession(options: SessionOptions): Promise<SessionInfo> {
    try {
      return sessionInfo(await this.#binding.createSession(nativeSessionOptions(options)))
    } catch (error) {
      throw translateError(error)
    }
  }

  async resumeSession(options: ResumeOptions): Promise<SessionInfo> {
    const nativeOptions: NativeResumeOptions = {
      ...nativeSessionOptions(options),
      rolloutPath: options.rolloutPath,
    }
    try {
      return sessionInfo(await this.#binding.resumeSession(nativeOptions))
    } catch (error) {
      throw translateError(error)
    }
  }

  async forkSession(options: ForkOptions): Promise<SessionInfo> {
    const nativeOptions: NativeForkOptions = { sourceSessionId: options.sourceSessionId }
    if (options.cwd !== undefined) nativeOptions.cwd = options.cwd
    if (options.provider !== undefined) nativeOptions.provider = options.provider
    if (options.model !== undefined) nativeOptions.model = options.model
    try {
      return sessionInfo(await this.#binding.forkSession(nativeOptions))
    } catch (error) {
      throw translateError(error)
    }
  }

  async submitTurn(sessionId: string, text: string): Promise<SubmissionInfo> {
    try {
      return submissionInfo(await this.#binding.submitTurn(sessionId, text))
    } catch (error) {
      throw translateError(error)
    }
  }

  async steer(options: SteerOptions): Promise<SubmissionInfo> {
    try {
      return submissionInfo(await this.#binding.steer({ ...options }))
    } catch (error) {
      throw translateError(error)
    }
  }

  async interrupt(sessionId: string): Promise<string> {
    try {
      return await this.#binding.interrupt(sessionId)
    } catch (error) {
      throw translateError(error)
    }
  }

  async resolveApproval(response: ApprovalResponse): Promise<string> {
    try {
      return await this.#binding.resolveApproval(nativeApprovalResponse(response))
    } catch (error) {
      throw translateError(error)
    }
  }

  async resolveDynamicTool(response: DynamicToolResponse): Promise<string> {
    try {
      return await this.#binding.resolveDynamicTool(nativeDynamicToolResponse(response))
    } catch (error) {
      throw translateError(error)
    }
  }

  async executeGovernedCommand(request: GovernedCommandRequest): Promise<GovernedCommandResult> {
    try {
      return governedCommandResult(
        await this.#binding.executeGovernedCommand(nativeGovernedCommandRequest(request)),
      )
    } catch (error) {
      throw translateError(error)
    }
  }

  async cancelGovernedCommand(sessionId: string, commandId: string): Promise<void> {
    try {
      await this.#binding.cancelGovernedCommand(sessionId, commandId)
    } catch (error) {
      throw translateError(error)
    }
  }

  async pollEvent(sessionId: string, timeoutMillis?: number): Promise<EventPoll> {
    if (timeoutMillis !== undefined) validateUint32(timeoutMillis, 'timeoutMillis')
    try {
      return eventPoll(await this.#binding.nextEvent(sessionId, timeoutMillis))
    } catch (error) {
      throw translateError(error)
    }
  }

  async *events(
    sessionId: string,
    options: EventStreamOptions = {},
  ): AsyncGenerator<KernelEvent, void, undefined> {
    if (this.#eventSubscribers.has(sessionId)) {
      throw new KernelError(
        'EVENT_SUBSCRIBER_EXISTS',
        `session ${sessionId} already has an active event subscriber`,
      )
    }
    const timeoutMillis = options.timeoutMillis ?? DEFAULT_EVENT_POLL_MILLIS
    validateUint32(timeoutMillis, 'timeoutMillis')
    this.#eventSubscribers.add(sessionId)
    let previousSequence: bigint | undefined
    try {
      while (options.signal?.aborted !== true) {
        const poll = await this.pollEvent(sessionId, timeoutMillis)
        if (poll.status === 'closed') return
        if (poll.status === 'timeout') continue
        if (previousSequence !== undefined && poll.event.sequence <= previousSequence) {
          throw new KernelError(
            'EVENT_SEQUENCE_INVALID',
            `session ${sessionId} returned non-increasing event sequence`,
          )
        }
        previousSequence = poll.event.sequence
        yield poll.event
      }
    } finally {
      this.#eventSubscribers.delete(sessionId)
    }
  }

  async listSessions(): Promise<readonly string[]> {
    try {
      return Object.freeze([...(await this.#binding.listSessions())])
    } catch (error) {
      throw translateError(error)
    }
  }

  async closeSession(sessionId: string): Promise<void> {
    try {
      await this.#binding.closeSession(sessionId)
    } catch (error) {
      throw translateError(error)
    }
  }

  shutdown(): Promise<ShutdownInfo> {
    this.#shutdownPromise ??= this.#shutdown()
    return this.#shutdownPromise
  }

  async #shutdown(): Promise<ShutdownInfo> {
    try {
      const result = await this.#binding.shutdown()
      return Object.freeze({
        completed: Object.freeze([...result.completed]),
        submitFailed: Object.freeze([...result.submitFailed]),
        timedOut: Object.freeze([...result.timedOut]),
      })
    } catch (error) {
      throw translateError(error)
    }
  }

  #openModelStream(payloadJson: string): ReadableStream<string> {
    let request: ModelPortRequest
    try {
      request = parseModelPortRequest(payloadJson)
    } catch (error) {
      return new ReadableStream<string>({
        start(controller) {
          controller.enqueue(errorMessage(structuredModelPortFailure(error)))
          controller.close()
        },
      })
    }
    if (this.#modelOperations.has(request.requestId)) {
      return new ReadableStream<string>({
        start(controller) {
          controller.enqueue(errorMessage({
            code: 'MODEL_PORT_REQUEST_DUPLICATE',
            message: 'model request identifier is already active',
          }))
          controller.close()
        },
      })
    }
    const operation: ActiveModelOperation = {
      abortController: new AbortController(),
    }
    this.#modelOperations.set(request.requestId, operation)
    return new ReadableStream<string>({
      start: (controller) => {
        void this.#pumpModelStream(request, operation, controller)
      },
      cancel: () => {
        this.#cancelModelStream(request.requestId)
      },
    })
  }

  async #pumpModelStream(
    request: ModelPortRequest,
    operation: ActiveModelOperation,
    controller: ReadableStreamDefaultController<string>,
  ): Promise<void> {
    try {
      const iterable = this.#modelPort.stream(request, operation.abortController.signal)
      if (!isRecord(iterable) || typeof iterable[Symbol.asyncIterator] !== 'function') {
        throw new ModelPortError({
          code: 'MODEL_PORT_PROTOCOL_INVALID',
          message: 'model port did not return an async iterable',
        })
      }
      const iterator = iterable[Symbol.asyncIterator]()
      operation.iterator = iterator
      while (!operation.abortController.signal.aborted) {
        const result = await iterator.next()
        if (result.done) break
        controller.enqueue(serializeModelPortMessage(result.value))
      }
      controller.close()
    } catch (error) {
      if (!operation.abortController.signal.aborted) {
        try {
          controller.enqueue(errorMessage(structuredModelPortFailure(error)))
          controller.close()
        } catch {
          // The native reader may have been released while DSH was settling cancellation.
        }
      } else {
        try {
          controller.close()
        } catch {
          // ReadableStream cancellation already closed the controller.
        }
      }
    } finally {
      if (this.#modelOperations.get(request.requestId) === operation) {
        this.#modelOperations.delete(request.requestId)
      }
    }
  }

  #cancelModelStream(requestId: string): void {
    const operation = this.#modelOperations.get(requestId)
    if (operation === undefined || operation.abortController.signal.aborted) return
    operation.abortController.abort()
    const returned = operation.iterator?.return?.()
    if (returned !== undefined) void returned.catch(() => undefined)
  }
}

export const nativeComponent: WorkspaceComponentDescriptor = Object.freeze({
  name: '@winwincode/native',
  kind: 'native-interface',
})
