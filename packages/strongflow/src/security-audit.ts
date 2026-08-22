import { createHash, randomUUID } from 'node:crypto'
import {
  mkdir,
  open,
  readFile,
  rename,
  rm,
  stat,
} from 'node:fs/promises'
import { join, resolve } from 'node:path'

export const STRONGFLOW_SECURITY_AUDIT_SCHEMA_VERSION = 1 as const

const MAX_AUDIT_EVENT_BYTES = 256 * 1024
const MAX_AUDIT_TEXT_BYTES = 16 * 1024
const LOCK_WAIT_MILLIS = 5_000
const STALE_LOCK_MILLIS = 30_000
const SENSITIVE_KEY = /(?:api[-_]?key|auth|credential|password|private[-_]?key|secret|token)/iu
const BEARER_VALUE = /\bBearer\s+[A-Za-z0-9._~+/=-]+/giu
const SENSITIVE_ASSIGNMENT = /\b(?:api[-_]?key|auth|credential|password|private[-_]?key|secret|token)\s*=\s*[^\s,;]+/giu
const JSON_WEB_TOKEN = /\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/gu
const PRIVATE_KEY_BLOCK = /-----BEGIN [^-\r\n]*PRIVATE KEY-----[\s\S]*?-----END [^-\r\n]*PRIVATE KEY-----/gu
const BASIC_AUTH_VALUES = /\bBasic\s+[A-Za-z0-9+/]{12,}={0,2}\b/giu
const PROVIDER_SECRET_VALUES = /\b(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{35}|xox[baprs]-[A-Za-z0-9-]{10,}|npm_[A-Za-z0-9]{20,})\b/gu
const URL_USERINFO_VALUES = /\b(?:https?|wss?):\/\/[^/\s:@]+:[^/\s@]+@/giu
const SENSITIVE_PROPERTY = /((?:(?:^|[{,]\s*)(?:"(?:api[-_]?key|auth(?:entication|orization)?|authorization|credential(?:s)?|password|passwd|private[-_]?key|secret|access[-_]?token|refresh[-_]?token|id[-_]?token|session[-_]?token|client[-_]?secret|token)"|'(?:api[-_]?key|auth(?:entication|orization)?|authorization|credential(?:s)?|password|passwd|private[-_]?key|secret|access[-_]?token|refresh[-_]?token|id[-_]?token|session[-_]?token|client[-_]?secret|token)')|(?:[{,]\s*)(?:api[-_]?key|auth(?:entication|orization)?|authorization|credential(?:s)?|password|passwd|private[-_]?key|secret|access[-_]?token|refresh[-_]?token|id[-_]?token|session[-_]?token|client[-_]?secret|token))\s*:\s*)(?:"[^"]*"|'[^']*'|\[[^\]]+\]|[^\s,}\]]+)/giu
const CREDENTIAL_VALUE_KEY = /^(?:api[-_]?key|auth(?:entication|orization)?|authorization|credential(?:s)?|password|passwd|private[-_]?key|secret|access[-_]?token|refresh[-_]?token|id[-_]?token|session[-_]?token|client[-_]?secret|token)$/iu
const CREDENTIAL_ASSIGNMENT = /\b(?:api[-_]?key|auth(?:entication|orization)?|authorization|credential(?:s)?|password|passwd|private[-_]?key|secret|access[-_]?token|refresh[-_]?token|id[-_]?token|session[-_]?token|client[-_]?secret|token)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s,;]+))/giu
const CREDENTIAL_PROPERTY = /(?:(?:^|[{,]\s*)(?:"[^"]+"|'[^']+')|(?:[{,]\s*)[A-Za-z0-9_-]+)\s*:\s*(?:"([^"]*)"|'([^']*)'|(\[[^\]]+\]|[^\s,}\]]+))/giu
const BASIC_AUTH_VALUE = /\bBasic\s+[A-Za-z0-9+/]{12,}={0,2}\b/iu
const PROVIDER_SECRET_VALUE = /\b(?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{35}|xox[baprs]-[A-Za-z0-9-]{10,}|npm_[A-Za-z0-9]{20,})\b/u
const URL_USERINFO_VALUE = /\b(?:https?|wss?):\/\/[^/\s:@]+:[^/\s@]+@/iu
const SAFE_CREDENTIAL_LITERAL = /^(?:\[REDACTED(?: [A-Z ]+)?\]|<redacted>|redacted|null|undefined|none|dsh-reference-only|credential-reference|reference-only)$/iu
const CREDENTIAL_PLACEHOLDER = /^(?:\$\{?|<)?(?:api_?key|apikey|auth(?:orization)?|credential|password|private_?key|secret|token)(?:\}|>)?$/iu
const SECURITY_EVENT_TYPES = new Set<StrongFlowSecurityAuditEventType>([
  'strongflow.security.session.accepted',
  'strongflow.security.tool.requested',
  'strongflow.security.tool.denied',
  'strongflow.security.tool.completed',
  'strongflow.security.tool.failed',
  'strongflow.security.process.authorized',
  'strongflow.security.process.denied',
  'strongflow.security.process.completed',
  'strongflow.security.process.failed',
  'strongflow.security.approval.requested',
  'strongflow.security.approval.decided',
  'strongflow.security.credential.boundary',
])
const SECURITY_OUTCOMES = new Set<StrongFlowSecurityAuditEvent['outcome']>([
  'accepted',
  'requested',
  'authorized',
  'completed',
  'policy-denied',
  'sandbox-denied',
  'task-failed',
  'approved',
  'rejected',
  'cancelled',
  'unavailable',
])

export type StrongFlowSecurityAuditEventType =
  | 'strongflow.security.session.accepted'
  | 'strongflow.security.tool.requested'
  | 'strongflow.security.tool.denied'
  | 'strongflow.security.tool.completed'
  | 'strongflow.security.tool.failed'
  | 'strongflow.security.process.authorized'
  | 'strongflow.security.process.denied'
  | 'strongflow.security.process.completed'
  | 'strongflow.security.process.failed'
  | 'strongflow.security.approval.requested'
  | 'strongflow.security.approval.decided'
  | 'strongflow.security.credential.boundary'

export interface StrongFlowSecurityAuditSource {
  readonly authority: 'codex-core'
  readonly kernelSessionLineageId: string
  readonly kernelSessionId: string
  readonly kernelStreamId: string
  readonly kernelSequence: string | null
  readonly turnId: string | null
  readonly operationId: string | null
}

/** Credential-free security fact emitted at every governed authority decision. */
export interface StrongFlowSecurityAuditEvent {
  readonly schemaVersion: typeof STRONGFLOW_SECURITY_AUDIT_SCHEMA_VERSION
  readonly type: StrongFlowSecurityAuditEventType
  readonly jobId: string
  readonly stageRunId: string
  readonly attemptId: string
  readonly roleId: string
  readonly contextId: string
  readonly source: StrongFlowSecurityAuditSource
  readonly outcome:
    | 'accepted'
    | 'requested'
    | 'authorized'
    | 'completed'
    | 'policy-denied'
    | 'sandbox-denied'
    | 'task-failed'
    | 'approved'
    | 'rejected'
    | 'cancelled'
    | 'unavailable'
  readonly facts: Readonly<Record<string, unknown>>
}

export interface StrongFlowSecurityAuditSink {
  append(event: StrongFlowSecurityAuditEvent): Promise<void> | void
}

export interface StrongFlowStoredSecurityAuditRecord {
  readonly schemaVersion: typeof STRONGFLOW_SECURITY_AUDIT_SCHEMA_VERSION
  readonly sequence: string
  readonly previousDigest: string | null
  readonly event: StrongFlowSecurityAuditEvent
  readonly digest: string
}

export type StrongFlowSecurityAuditErrorCode =
  | 'INVALID_AUDIT_OPTIONS'
  | 'INVALID_AUDIT_EVENT'
  | 'AUDIT_LOCK_TIMEOUT'
  | 'AUDIT_CORRUPT'
  | 'AUDIT_IO_FAILED'

export class StrongFlowSecurityAuditError extends Error {
  readonly code: StrongFlowSecurityAuditErrorCode

  constructor(
    code: StrongFlowSecurityAuditErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowSecurityAuditError'
    this.code = code
  }
}

export interface DurableStrongFlowSecurityAuditOptions {
  readonly home: string
  readonly sensitiveValues?: readonly string[]
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function errorCode(error: unknown): string | undefined {
  if (typeof error !== 'object' || error === null || !('code' in error)) return undefined
  return typeof error.code === 'string' ? error.code : undefined
}

function redactText(value: string, sensitiveValues: readonly string[]): string {
  let redacted = value
  for (const sensitive of sensitiveValues) {
    if (sensitive.length > 0) redacted = redacted.replaceAll(sensitive, '[REDACTED]')
  }
  redacted = redacted
    .replace(PRIVATE_KEY_BLOCK, '[REDACTED PRIVATE KEY]')
    .replace(BEARER_VALUE, 'Bearer [REDACTED]')
    .replace(BASIC_AUTH_VALUES, 'Basic [REDACTED]')
    .replace(PROVIDER_SECRET_VALUES, '[REDACTED CREDENTIAL]')
    .replace(URL_USERINFO_VALUES, '[REDACTED URL CREDENTIAL]')
    .replace(SENSITIVE_ASSIGNMENT, match => `${match.split('=', 1)[0]}=[REDACTED]`)
    .replace(SENSITIVE_PROPERTY, (_match, prefix: string) => `${prefix}"[REDACTED]"`)
    .replace(JSON_WEB_TOKEN, '[REDACTED JWT]')
  if (Buffer.byteLength(redacted) <= MAX_AUDIT_TEXT_BYTES) return redacted
  return `${Buffer.from(redacted).subarray(0, MAX_AUDIT_TEXT_BYTES).toString('utf8')}…`
}

function safeCredentialLiteral(value: unknown): boolean {
  if (value === null || value === undefined || value === false || value === '') return true
  return typeof value === 'string'
    && (SAFE_CREDENTIAL_LITERAL.test(value.trim()) || CREDENTIAL_PLACEHOLDER.test(value.trim()))
}

function capturedCredentialValue(match: RegExpMatchArray): string {
  return match[1] ?? match[2] ?? match[3] ?? ''
}

function textContainsCredentialMaterial(
  value: string,
  sensitiveValues: readonly string[],
): boolean {
  if (sensitiveValues.some(sensitive => sensitive.length > 0 && value.includes(sensitive))) {
    return true
  }
  if (
    /-----BEGIN [^-\r\n]*PRIVATE KEY-----[\s\S]*?-----END [^-\r\n]*PRIVATE KEY-----/u.test(value)
    || /\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/u.test(value)
    || /\bBearer\s+(?!\[REDACTED\])[A-Za-z0-9._~+/=-]+/iu.test(value)
    || BASIC_AUTH_VALUE.test(value)
    || PROVIDER_SECRET_VALUE.test(value)
    || URL_USERINFO_VALUE.test(value)
  ) return true
  for (const match of value.matchAll(CREDENTIAL_ASSIGNMENT)) {
    if (!safeCredentialLiteral(capturedCredentialValue(match))) return true
  }
  const propertyText = value.replaceAll(/\bBearer\s+\[REDACTED\]/giu, '[REDACTED]')
  for (const match of propertyText.matchAll(CREDENTIAL_PROPERTY)) {
    const property = match[0].split(':', 1)[0]
      ?.replace(/^[{,]\s*/u, '')
      .replaceAll(/["']/gu, '')
      .trim()
    if (
      property !== undefined
      && CREDENTIAL_VALUE_KEY.test(property)
      && !safeCredentialLiteral(capturedCredentialValue(match))
    ) return true
  }
  return false
}

/** Return true when a value contains raw credential material rather than a safe reference. */
export function containsStrongFlowCredentialMaterial(
  value: unknown,
  sensitiveValues: readonly string[] = [],
): boolean {
  const seen = new WeakSet<object>()
  const walk = (input: unknown, key = '', depth = 0): boolean => {
    if (depth > 64) return true
    if (CREDENTIAL_VALUE_KEY.test(key) && !safeCredentialLiteral(input)) return true
    if (typeof input === 'string') {
      return textContainsCredentialMaterial(input, sensitiveValues)
    }
    if (
      typeof input === 'number'
      || typeof input === 'boolean'
      || typeof input === 'bigint'
      || input === null
      || input === undefined
    ) return false
    if (typeof input !== 'object') return true
    if (input instanceof Uint8Array) {
      return textContainsCredentialMaterial(Buffer.from(input).toString('utf8'), sensitiveValues)
    }
    if (seen.has(input)) return true
    seen.add(input)
    if (Array.isArray(input)) return input.some(entry => walk(entry, '', depth + 1))
    if (!isRecord(input)) return true
    return Object.entries(input).some(([childKey, child]) => walk(child, childKey, depth + 1))
  }
  return walk(value)
}

/** Deeply redact values before they can enter a model response, diagnostic, or durable record. */
export function redactStrongFlowSecurityValue(
  value: unknown,
  sensitiveValues: readonly string[] = [],
): unknown {
  const seen = new WeakSet<object>()
  const walk = (input: unknown, key = '', depth = 0): unknown => {
    if (SENSITIVE_KEY.test(key)) return '[REDACTED]'
    if (depth > 32) return '[REDACTED DEPTH]'
    if (typeof input === 'string') return redactText(input, sensitiveValues)
    if (
      typeof input === 'number'
      || typeof input === 'boolean'
      || input === null
      || input === undefined
    ) return input ?? null
    if (typeof input === 'bigint') return input.toString()
    if (typeof input !== 'object') return String(input)
    if (seen.has(input)) return '[REDACTED CYCLE]'
    seen.add(input)
    if (Array.isArray(input)) {
      return Object.freeze(input.map(entry => walk(entry, '', depth + 1)))
    }
    if (!isRecord(input)) return `[${input.constructor?.name ?? 'Object'}]`
    return Object.freeze(Object.fromEntries(Object.entries(input).map(([childKey, child]) => (
      [childKey, walk(child, childKey, depth + 1)]
    ))))
  }
  return walk(value)
}

/** SHA-256 identity for content that must be auditable but must not be retained. */
export function strongFlowSecurityDigestText(value: string): string {
  return `sha256:${createHash('sha256').update(value).digest('hex')}`
}

function normalizedEvent(
  event: StrongFlowSecurityAuditEvent,
  sensitiveValues: readonly string[],
): StrongFlowSecurityAuditEvent {
  if (
    !isRecord(event)
    || event.schemaVersion !== STRONGFLOW_SECURITY_AUDIT_SCHEMA_VERSION
    || !SECURITY_EVENT_TYPES.has(event.type)
    || !SECURITY_OUTCOMES.has(event.outcome)
    || Object.keys(event).some(key => ![
      'schemaVersion',
      'type',
      'jobId',
      'stageRunId',
      'attemptId',
      'roleId',
      'contextId',
      'source',
      'outcome',
      'facts',
    ].includes(key))
    || !isRecord(event.source)
    || event.source.authority !== 'codex-core'
    || Object.keys(event.source).some(key => ![
      'authority',
      'kernelSessionLineageId',
      'kernelSessionId',
      'kernelStreamId',
      'kernelSequence',
      'turnId',
      'operationId',
    ].includes(key))
    || [
      event.source.kernelSequence,
      event.source.turnId,
      event.source.operationId,
    ].some(entry => entry !== null && (typeof entry !== 'string' || entry.length === 0))
    || !isRecord(event.facts)
    || [
      event.jobId,
      event.stageRunId,
      event.attemptId,
      event.roleId,
      event.contextId,
      event.source.kernelSessionLineageId,
      event.source.kernelSessionId,
      event.source.kernelStreamId,
    ].some(entry => typeof entry !== 'string' || entry.length === 0)
  ) throw new StrongFlowSecurityAuditError(
    'INVALID_AUDIT_EVENT',
    'StrongFlow security audit event is incomplete',
  )
  const redacted = redactStrongFlowSecurityValue(event, sensitiveValues)
  if (!isRecord(redacted) || !isRecord(redacted.source) || !isRecord(redacted.facts)) {
    throw new StrongFlowSecurityAuditError(
      'INVALID_AUDIT_EVENT',
      'StrongFlow security audit event could not be redacted',
    )
  }
  const serialized = JSON.stringify(redacted)
  if (Buffer.byteLength(serialized) > MAX_AUDIT_EVENT_BYTES) {
    return Object.freeze({
      ...redacted,
      facts: Object.freeze({
        summary: 'Security facts exceeded the durable audit size limit.',
      }),
    }) as unknown as StrongFlowSecurityAuditEvent
  }
  return redacted as unknown as StrongFlowSecurityAuditEvent
}

function recordDigest(
  sequence: string,
  previousDigest: string | null,
  event: StrongFlowSecurityAuditEvent,
): string {
  return strongFlowSecurityDigestText(JSON.stringify({
    schemaVersion: STRONGFLOW_SECURITY_AUDIT_SCHEMA_VERSION,
    sequence,
    previousDigest,
    event,
  }))
}

function parseRecords(text: string): StrongFlowStoredSecurityAuditRecord[] {
  if (text.length === 0) return []
  const lines = text.endsWith('\n') ? text.slice(0, -1).split('\n') : text.split('\n')
  const records: StrongFlowStoredSecurityAuditRecord[] = []
  let previousDigest: string | null = null
  for (const [index, line] of lines.entries()) {
    let value: unknown
    try {
      value = JSON.parse(line) as unknown
    } catch (error) {
      throw new StrongFlowSecurityAuditError(
        'AUDIT_CORRUPT',
        `security audit record ${index + 1} is not JSON`,
        { cause: error },
      )
    }
    const sequence = (index + 1).toString()
    if (
      !isRecord(value)
      || value.schemaVersion !== STRONGFLOW_SECURITY_AUDIT_SCHEMA_VERSION
      || value.sequence !== sequence
      || value.previousDigest !== previousDigest
      || !isRecord(value.event)
      || typeof value.digest !== 'string'
      || value.digest !== recordDigest(
        sequence,
        previousDigest,
        value.event as unknown as StrongFlowSecurityAuditEvent,
      )
    ) throw new StrongFlowSecurityAuditError(
      'AUDIT_CORRUPT',
      `security audit record ${sequence} broke its append-only digest chain`,
    )
    const record: StrongFlowStoredSecurityAuditRecord = Object.freeze({
      schemaVersion: STRONGFLOW_SECURITY_AUDIT_SCHEMA_VERSION,
      sequence,
      previousDigest,
      event: value.event as unknown as StrongFlowSecurityAuditEvent,
      digest: value.digest,
    })
    records.push(record)
    previousDigest = record.digest
  }
  return records
}

async function syncDirectory(path: string): Promise<void> {
  const handle = await open(path, 'r')
  try {
    await handle.sync()
  } finally {
    await handle.close()
  }
}

async function delay(millis: number): Promise<void> {
  await new Promise(resolvePromise => setTimeout(resolvePromise, millis))
}

/** Durable, per-job, append-only JSONL security audit with a verified SHA-256 chain. */
export class DurableStrongFlowSecurityAudit implements StrongFlowSecurityAuditSink {
  readonly #root: string
  readonly #sensitiveValues: readonly string[]

  constructor(options: DurableStrongFlowSecurityAuditOptions) {
    if (
      !isRecord(options)
      || typeof options.home !== 'string'
      || options.home.length === 0
      || (options.sensitiveValues !== undefined && (
        !Array.isArray(options.sensitiveValues)
        || options.sensitiveValues.some(value => typeof value !== 'string' || value.length === 0)
      ))
    ) throw new StrongFlowSecurityAuditError(
      'INVALID_AUDIT_OPTIONS',
      'StrongFlow security audit requires a home and optional non-empty sensitive values',
    )
    this.#root = join(resolve(options.home), 'strongflow-security-audit')
    this.#sensitiveValues = Object.freeze([...(options.sensitiveValues ?? [])])
  }

  async append(event: StrongFlowSecurityAuditEvent): Promise<void> {
    const normalized = normalizedEvent(event, this.#sensitiveValues)
    const directory = this.#jobDirectory(normalized.jobId)
    try {
      await mkdir(directory, { recursive: true, mode: 0o700 })
      await this.#withLock(directory, async () => {
        const path = join(directory, 'security.jsonl')
        const existing = await this.#readPath(path)
        const previous = existing.at(-1)
        const sequence = (existing.length + 1).toString()
        const previousDigest = previous?.digest ?? null
        const record = Object.freeze({
          schemaVersion: STRONGFLOW_SECURITY_AUDIT_SCHEMA_VERSION,
          sequence,
          previousDigest,
          event: normalized,
          digest: recordDigest(sequence, previousDigest, normalized),
        })
        const handle = await open(path, 'a', 0o600)
        try {
          await handle.write(`${JSON.stringify(record)}\n`, undefined, 'utf8')
          await handle.sync()
        } finally {
          await handle.close()
        }
        await syncDirectory(directory)
      })
    } catch (error) {
      if (error instanceof StrongFlowSecurityAuditError) throw error
      throw new StrongFlowSecurityAuditError(
        'AUDIT_IO_FAILED',
        'StrongFlow security audit could not append its durable record',
        { cause: error },
      )
    }
  }

  async read(jobId: string): Promise<readonly StrongFlowStoredSecurityAuditRecord[]> {
    if (typeof jobId !== 'string' || jobId.length === 0) {
      throw new StrongFlowSecurityAuditError('INVALID_AUDIT_OPTIONS', 'job id is empty')
    }
    const directory = this.#jobDirectory(jobId)
    try {
      await mkdir(directory, { recursive: true, mode: 0o700 })
      return await this.#withLock(
        directory,
        async () => Object.freeze(await this.#readPath(join(directory, 'security.jsonl'))),
      )
    } catch (error) {
      if (error instanceof StrongFlowSecurityAuditError) throw error
      throw new StrongFlowSecurityAuditError(
        'AUDIT_IO_FAILED',
        'StrongFlow security audit could not read its durable records',
        { cause: error },
      )
    }
  }

  #jobDirectory(jobId: string): string {
    const key = createHash('sha256').update(jobId).digest('hex')
    return join(this.#root, key)
  }

  async #readPath(path: string): Promise<StrongFlowStoredSecurityAuditRecord[]> {
    try {
      return parseRecords(await readFile(path, 'utf8'))
    } catch (error) {
      if (errorCode(error) === 'ENOENT') return []
      throw error
    }
  }

  async #withLock<Value>(directory: string, operation: () => Promise<Value>): Promise<Value> {
    const lockPath = join(directory, 'append.lock')
    const deadline = Date.now() + LOCK_WAIT_MILLIS
    while (true) {
      try {
        await mkdir(lockPath, { mode: 0o700 })
        break
      } catch (error) {
        if (errorCode(error) !== 'EEXIST') throw error
        try {
          const lockStat = await stat(lockPath)
          if (Date.now() - lockStat.mtimeMs > STALE_LOCK_MILLIS) {
            const stalePath = `${lockPath}.stale-${randomUUID()}`
            await rename(lockPath, stalePath)
            await rm(stalePath, { recursive: true, force: true })
            continue
          }
        } catch (lockError) {
          if (errorCode(lockError) === 'ENOENT') continue
          throw lockError
        }
        if (Date.now() >= deadline) {
          throw new StrongFlowSecurityAuditError(
            'AUDIT_LOCK_TIMEOUT',
            'StrongFlow security audit append lock did not become available',
          )
        }
        await delay(10)
      }
    }
    try {
      return await operation()
    } finally {
      await rm(lockPath, { recursive: true, force: true })
    }
  }
}
