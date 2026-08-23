import { createHash, randomUUID } from 'node:crypto'
import {
  link,
  lstat,
  mkdir,
  open,
  readFile,
  readdir,
  rename,
  rm,
} from 'node:fs/promises'
import { basename, join, resolve } from 'node:path'

import { containsRawCredentialMaterial } from './credential-boundary.js'
import {
  parseStrongFlowGitHubProviderOperation,
  type StrongFlowGitHubProviderOperation,
  type StrongFlowGitHubProviderOperationKind,
} from './github-publication-provider.js'

/** Append-only operational journal; canonical Delivery ownership is unchanged. */
export const STRONGFLOW_GITHUB_PUBLICATION_JOURNAL_SCHEMA_VERSION = 1 as const

export const STRONGFLOW_GITHUB_PUBLICATION_INTENT_PROTOCOL =
  'winwincode.github-publication-intent.v1' as const

export const STRONGFLOW_GITHUB_PUBLICATION_EVENT_PROTOCOL =
  'winwincode.github-publication-event.v1' as const

export const STRONGFLOW_GITHUB_PUBLICATION_EVENT_OUTCOMES = Object.freeze([
  'lookup-found-current',
  'lookup-found-stale',
  'lookup-absent',
  'lookup-unknown',
  'lookup-conflict',
  'apply-intent',
  'apply-applied',
  'apply-unknown',
  'apply-rejected',
] as const)

export type StrongFlowGitHubPublicationEventOutcome =
  typeof STRONGFLOW_GITHUB_PUBLICATION_EVENT_OUTCOMES[number]

export interface StrongFlowGitHubPublicationIntent {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_PUBLICATION_JOURNAL_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_GITHUB_PUBLICATION_INTENT_PROTOCOL
  readonly runId: string
  readonly providerIdempotencyKey: string
  readonly reviewPackageId: string
  readonly deliveryId: string
  readonly deliverySpecId: string
  readonly deliverySpecRevision: number
  readonly candidateRef: string
  readonly deliveryVerdictId: string
  readonly publicationSetSha256: string
  readonly operations: readonly StrongFlowGitHubProviderOperation[]
  readonly createdAtMillis: number
  readonly digest: string
}

export interface StrongFlowGitHubPublicationEvent {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_PUBLICATION_JOURNAL_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_GITHUB_PUBLICATION_EVENT_PROTOCOL
  readonly eventId: string
  readonly runId: string
  readonly operationKey: string
  readonly requestSha256: string
  readonly outcome: StrongFlowGitHubPublicationEventOutcome
  readonly code: string | null
  readonly resourceRef: string | null
  readonly remoteWritePerformed: boolean | null
  readonly recordedAtMillis: number
  readonly digest: string
}

export type StrongFlowGitHubPublicationStepState =
  | 'pending'
  | 'succeeded'
  | 'failed'
  | 'unknown'

export interface StrongFlowGitHubPublicationStepProjection {
  readonly kind: StrongFlowGitHubProviderOperationKind
  readonly operationKey: string
  readonly requestSha256: string
  readonly state: StrongFlowGitHubPublicationStepState
  readonly resourceRef: string | null
  readonly lastCode: string | null
  readonly applyAttempts: number
  readonly confirmedRemoteWriteCount: number
}

export interface StrongFlowGitHubPublicationJournalProjection {
  readonly intent: StrongFlowGitHubPublicationIntent
  readonly events: readonly StrongFlowGitHubPublicationEvent[]
  readonly steps: readonly StrongFlowGitHubPublicationStepProjection[]
  readonly status: 'pending' | 'succeeded' | 'failed' | 'unknown'
  readonly confirmedRemoteWriteCount: number
}

export interface CreateStrongFlowGitHubPublicationIntentInput {
  readonly home: string
  readonly providerIdempotencyKey: string
  readonly reviewPackageId: string
  readonly deliveryId: string
  readonly deliverySpecId: string
  readonly deliverySpecRevision: number
  readonly candidateRef: string
  readonly deliveryVerdictId: string
  readonly publicationSetSha256: string
  readonly operations: readonly StrongFlowGitHubProviderOperation[]
  readonly createdAtMillis: number
}

export interface AppendStrongFlowGitHubPublicationEventInput {
  readonly operation: StrongFlowGitHubProviderOperation
  readonly outcome: StrongFlowGitHubPublicationEventOutcome
  readonly code: string | null
  readonly resourceRef: string | null
  readonly remoteWritePerformed: boolean | null
  readonly recordedAtMillis: number
}

export type StrongFlowGitHubPublicationJournalErrorCode =
  | 'INVALID_JOURNAL_INPUT'
  | 'JOURNAL_CONFLICT'
  | 'JOURNAL_CORRUPT'
  | 'JOURNAL_IO_ERROR'

export class StrongFlowGitHubPublicationJournalError extends Error {
  readonly code: StrongFlowGitHubPublicationJournalErrorCode

  constructor(
    code: StrongFlowGitHubPublicationJournalErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowGitHubPublicationJournalError'
    this.code = code
  }
}

const SHA256_PATTERN = /^[a-f0-9]{64}$/u
const RUN_ID_PATTERN = /^github-publication-run:sha256:[a-f0-9]{64}$/u
const PROVIDER_KEY_PATTERN = /^github:pull-request:sha256:[a-f0-9]{64}$/u
const PACKAGE_ID_PATTERN = /^github-review-package:sha256:[a-f0-9]{64}$/u
const CANDIDATE_REF_PATTERN = /^git-candidate:sha256:[a-f0-9]{64}$/u
const PORTABLE_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u
const PROVIDER_CODE_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,99}$/u
const EVENT_FILE_PATTERN = /^(github-publication-event:sha256:[a-f0-9]{64})\.json$/u
const PENDING_FILE_PATTERN = /^\.pending-[0-9a-f-]+\.json$/u
const MAX_RESOURCE_REF_LENGTH = 8_192

function journalError(
  code: StrongFlowGitHubPublicationJournalErrorCode,
  message: string,
  cause?: unknown,
): never {
  throw new StrongFlowGitHubPublicationJournalError(
    code,
    message,
    cause === undefined ? undefined : { cause },
  )
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  keys: readonly string[],
  label: string,
): void {
  const expected = new Set(keys)
  if (Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))) {
    return journalError('JOURNAL_CORRUPT', `${label} has an unexpected shape`)
  }
}

function digest(value: unknown): string {
  return createHash('sha256').update(JSON.stringify(value)).digest('hex')
}

function nonNegativeInteger(value: unknown, label: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Object.is(value, -0)) {
    return journalError('JOURNAL_CORRUPT', `${label} must be a non-negative safe integer`)
  }
  return Number(value)
}

function positiveInteger(value: unknown, label: string): number {
  const parsed = nonNegativeInteger(value, label)
  if (parsed < 1) return journalError('JOURNAL_CORRUPT', `${label} must be positive`)
  return parsed
}

function pattern(value: unknown, expected: RegExp, label: string): string {
  if (typeof value !== 'string' || !expected.test(value)) {
    return journalError('JOURNAL_CORRUPT', `${label} is invalid`)
  }
  return value
}

function nullableCode(value: unknown): string | null {
  if (value === null) return null
  return pattern(value, PROVIDER_CODE_PATTERN, 'publication event code')
}

function nullableResource(value: unknown): string | null {
  if (value === null) return null
  if (typeof value !== 'string'
    || value.trim().length === 0
    || value.length > MAX_RESOURCE_REF_LENGTH
    || /[\u0000-\u001f\u007f]/u.test(value)) {
    return journalError('JOURNAL_CORRUPT', 'publication event resourceRef is invalid')
  }
  if (containsRawCredentialMaterial(value)) {
    return journalError('JOURNAL_CORRUPT', 'publication event resourceRef contains credentials')
  }
  return value
}

function errorCode(error: unknown): string | undefined {
  if (typeof error !== 'object' || error === null || !('code' in error)) return undefined
  return typeof error.code === 'string' ? error.code : undefined
}

function intentWithoutDigest(
  value: Omit<StrongFlowGitHubPublicationIntent, 'digest'>,
): Omit<StrongFlowGitHubPublicationIntent, 'digest'> {
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_JOURNAL_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PUBLICATION_INTENT_PROTOCOL,
    runId: value.runId,
    providerIdempotencyKey: value.providerIdempotencyKey,
    reviewPackageId: value.reviewPackageId,
    deliveryId: value.deliveryId,
    deliverySpecId: value.deliverySpecId,
    deliverySpecRevision: value.deliverySpecRevision,
    candidateRef: value.candidateRef,
    deliveryVerdictId: value.deliveryVerdictId,
    publicationSetSha256: value.publicationSetSha256,
    operations: value.operations,
    createdAtMillis: value.createdAtMillis,
  })
}

function runIdentity(value: {
  readonly providerIdempotencyKey: string
  readonly reviewPackageId: string
  readonly publicationSetSha256: string
}): string {
  return `github-publication-run:sha256:${digest({
    providerIdempotencyKey: value.providerIdempotencyKey,
    reviewPackageId: value.reviewPackageId,
    publicationSetSha256: value.publicationSetSha256,
  })}`
}

function parseIntent(value: unknown): StrongFlowGitHubPublicationIntent {
  if (!isRecord(value)) return journalError('JOURNAL_CORRUPT', 'publication intent must be an object')
  exactKeys(value, [
    'schemaVersion',
    'protocol',
    'runId',
    'providerIdempotencyKey',
    'reviewPackageId',
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'candidateRef',
    'deliveryVerdictId',
    'publicationSetSha256',
    'operations',
    'createdAtMillis',
    'digest',
  ], 'publication intent')
  if (value.schemaVersion !== STRONGFLOW_GITHUB_PUBLICATION_JOURNAL_SCHEMA_VERSION
    || value.protocol !== STRONGFLOW_GITHUB_PUBLICATION_INTENT_PROTOCOL
    || !Array.isArray(value.operations)
    || value.operations.length !== 4) {
    return journalError('JOURNAL_CORRUPT', 'publication intent protocol is invalid')
  }
  const operations = Object.freeze(value.operations.map((entry, index) => (
    parseStrongFlowGitHubProviderOperation(entry, `publicationIntent.operations[${String(index)}]`)
  )))
  const parsed = intentWithoutDigest({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_JOURNAL_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PUBLICATION_INTENT_PROTOCOL,
    runId: pattern(value.runId, RUN_ID_PATTERN, 'publication intent runId'),
    providerIdempotencyKey: pattern(
      value.providerIdempotencyKey,
      PROVIDER_KEY_PATTERN,
      'publication intent provider key',
    ),
    reviewPackageId: pattern(
      value.reviewPackageId,
      PACKAGE_ID_PATTERN,
      'publication intent package id',
    ),
    deliveryId: pattern(value.deliveryId, PORTABLE_ID_PATTERN, 'publication intent delivery id'),
    deliverySpecId: pattern(
      value.deliverySpecId,
      PORTABLE_ID_PATTERN,
      'publication intent spec id',
    ),
    deliverySpecRevision: positiveInteger(
      value.deliverySpecRevision,
      'publication intent spec revision',
    ),
    candidateRef: pattern(
      value.candidateRef,
      CANDIDATE_REF_PATTERN,
      'publication intent candidate',
    ),
    deliveryVerdictId: pattern(
      value.deliveryVerdictId,
      PORTABLE_ID_PATTERN,
      'publication intent verdict id',
    ),
    publicationSetSha256: pattern(
      value.publicationSetSha256,
      SHA256_PATTERN,
      'publication intent set digest',
    ),
    operations,
    createdAtMillis: nonNegativeInteger(
      value.createdAtMillis,
      'publication intent createdAtMillis',
    ),
  })
  const parsedDigest = pattern(value.digest, SHA256_PATTERN, 'publication intent digest')
  if (parsed.runId !== runIdentity(parsed)
    || parsedDigest !== digest(parsed)
    || new Set(operations.map(operation => operation.kind)).size !== 4) {
    return journalError('JOURNAL_CORRUPT', 'publication intent identity changed')
  }
  return Object.freeze({ ...parsed, digest: parsedDigest })
}

function eventWithoutIdentity(
  value: Omit<StrongFlowGitHubPublicationEvent, 'eventId' | 'digest'>,
): Omit<StrongFlowGitHubPublicationEvent, 'eventId' | 'digest'> {
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_JOURNAL_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PUBLICATION_EVENT_PROTOCOL,
    runId: value.runId,
    operationKey: value.operationKey,
    requestSha256: value.requestSha256,
    outcome: value.outcome,
    code: value.code,
    resourceRef: value.resourceRef,
    remoteWritePerformed: value.remoteWritePerformed,
    recordedAtMillis: value.recordedAtMillis,
  })
}

function materializeEvent(
  runId: string,
  input: AppendStrongFlowGitHubPublicationEventInput,
): StrongFlowGitHubPublicationEvent {
  const unsigned = eventWithoutIdentity({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_JOURNAL_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PUBLICATION_EVENT_PROTOCOL,
    runId,
    operationKey: input.operation.operationKey,
    requestSha256: input.operation.requestSha256,
    outcome: input.outcome,
    code: input.code,
    resourceRef: input.resourceRef,
    remoteWritePerformed: input.remoteWritePerformed,
    recordedAtMillis: input.recordedAtMillis,
  })
  const eventId = `github-publication-event:sha256:${digest(unsigned)}`
  const withId = Object.freeze({ ...unsigned, eventId })
  return Object.freeze({ ...withId, digest: digest(withId) })
}

function parseEvent(value: unknown): StrongFlowGitHubPublicationEvent {
  if (!isRecord(value)) return journalError('JOURNAL_CORRUPT', 'publication event must be an object')
  exactKeys(value, [
    'schemaVersion',
    'protocol',
    'eventId',
    'runId',
    'operationKey',
    'requestSha256',
    'outcome',
    'code',
    'resourceRef',
    'remoteWritePerformed',
    'recordedAtMillis',
    'digest',
  ], 'publication event')
  if (value.schemaVersion !== STRONGFLOW_GITHUB_PUBLICATION_JOURNAL_SCHEMA_VERSION
    || value.protocol !== STRONGFLOW_GITHUB_PUBLICATION_EVENT_PROTOCOL
    || typeof value.outcome !== 'string'
    || !STRONGFLOW_GITHUB_PUBLICATION_EVENT_OUTCOMES.includes(
      value.outcome as StrongFlowGitHubPublicationEventOutcome,
    )
    || (value.remoteWritePerformed !== null
      && typeof value.remoteWritePerformed !== 'boolean')) {
    return journalError('JOURNAL_CORRUPT', 'publication event protocol is invalid')
  }
  const unsigned = eventWithoutIdentity({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_JOURNAL_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PUBLICATION_EVENT_PROTOCOL,
    runId: pattern(value.runId, RUN_ID_PATTERN, 'publication event runId'),
    operationKey: pattern(
      value.operationKey,
      /^github:pull-request:sha256:[a-f0-9]{64}:(?:branch|pull-request|issue-comment|commit-status)$/u,
      'publication event operation key',
    ),
    requestSha256: pattern(
      value.requestSha256,
      SHA256_PATTERN,
      'publication event request digest',
    ),
    outcome: value.outcome as StrongFlowGitHubPublicationEventOutcome,
    code: nullableCode(value.code),
    resourceRef: nullableResource(value.resourceRef),
    remoteWritePerformed: value.remoteWritePerformed as boolean | null,
    recordedAtMillis: nonNegativeInteger(
      value.recordedAtMillis,
      'publication event recordedAtMillis',
    ),
  })
  const eventId = pattern(
    value.eventId,
    /^github-publication-event:sha256:[a-f0-9]{64}$/u,
    'publication event id',
  )
  const withId = Object.freeze({ ...unsigned, eventId })
  const parsedDigest = pattern(value.digest, SHA256_PATTERN, 'publication event digest')
  if (eventId !== `github-publication-event:sha256:${digest(unsigned)}`
    || parsedDigest !== digest(withId)) {
    return journalError('JOURNAL_CORRUPT', 'publication event identity changed')
  }
  return Object.freeze({ ...withId, digest: parsedDigest })
}

function eventState(
  operation: StrongFlowGitHubProviderOperation,
  events: readonly StrongFlowGitHubPublicationEvent[],
): StrongFlowGitHubPublicationStepProjection {
  const matching = events.filter(event => (
    event.operationKey === operation.operationKey
    && event.requestSha256 === operation.requestSha256
  ))
  const succeeded = matching.filter(event => (
    event.outcome === 'lookup-found-current' || event.outcome === 'apply-applied'
  )).toSorted((left, right) => (
    right.recordedAtMillis - left.recordedAtMillis || right.eventId.localeCompare(left.eventId)
  ))[0]
  const last = matching.toSorted((left, right) => (
    right.recordedAtMillis - left.recordedAtMillis || right.eventId.localeCompare(left.eventId)
  ))[0]
  let state: StrongFlowGitHubPublicationStepState = 'pending'
  if (succeeded !== undefined) state = 'succeeded'
  else if (last?.outcome === 'lookup-conflict'
    || last?.outcome === 'apply-rejected') state = 'failed'
  else if (last?.outcome === 'lookup-unknown'
    || last?.outcome === 'apply-unknown'
    || last?.outcome === 'apply-intent') state = 'unknown'
  return Object.freeze({
    kind: operation.kind,
    operationKey: operation.operationKey,
    requestSha256: operation.requestSha256,
    state,
    resourceRef: succeeded?.resourceRef ?? null,
    lastCode: last?.code ?? null,
    applyAttempts: matching.filter(event => event.outcome === 'apply-intent').length,
    confirmedRemoteWriteCount: matching.filter(event => (
      event.outcome === 'apply-applied' && event.remoteWritePerformed === true
    )).length,
  })
}

function projection(
  intent: StrongFlowGitHubPublicationIntent,
  events: readonly StrongFlowGitHubPublicationEvent[],
): StrongFlowGitHubPublicationJournalProjection {
  const steps = Object.freeze(intent.operations.map(operation => eventState(operation, events)))
  const status = steps.every(step => step.state === 'succeeded')
    ? 'succeeded' as const
    : steps.some(step => step.state === 'failed')
      ? 'failed' as const
      : steps.some(step => step.state === 'unknown')
        ? 'unknown' as const
        : 'pending' as const
  return Object.freeze({
    intent,
    events: Object.freeze([...events]),
    steps,
    status,
    confirmedRemoteWriteCount: steps.reduce(
      (total, step) => total + step.confirmedRemoteWriteCount,
      0,
    ),
  })
}

function journalRoot(home: string): string {
  if (typeof home !== 'string' || home.length === 0) {
    return journalError('INVALID_JOURNAL_INPUT', 'publication home must be a non-empty path')
  }
  return join(resolve(home), 'github-publications')
}

function runDirectory(home: string, intent: StrongFlowGitHubPublicationIntent): string {
  return join(
    journalRoot(home),
    createHash('sha256').update(intent.providerIdempotencyKey).digest('hex'),
    createHash('sha256').update(intent.reviewPackageId).digest('hex'),
  )
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await lstat(path)
    return true
  } catch (error) {
    if (errorCode(error) === 'ENOENT') return false
    throw error
  }
}

async function syncDirectory(path: string): Promise<void> {
  const handle = await open(path, 'r')
  try {
    await handle.sync()
  } finally {
    await handle.close()
  }
}

async function writeDurable(path: string, content: string): Promise<void> {
  const handle = await open(path, 'wx', 0o600)
  try {
    await handle.writeFile(content, 'utf8')
    await handle.sync()
  } finally {
    await handle.close()
  }
}

function equal(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right)
}

/** Durable append-only store for provider intent and reconciliation facts. */
export class StrongFlowGitHubPublicationJournal {
  readonly home: string
  readonly directory: string
  readonly intentPath: string
  readonly eventsDirectory: string
  readonly intent: StrongFlowGitHubPublicationIntent
  #tail: Promise<void> = Promise.resolve()

  private constructor(
    home: string,
    directory: string,
    intent: StrongFlowGitHubPublicationIntent,
  ) {
    this.home = home
    this.directory = directory
    this.intentPath = join(directory, 'intent.json')
    this.eventsDirectory = join(directory, 'events')
    this.intent = intent
  }

  static async createOrOpen(
    input: CreateStrongFlowGitHubPublicationIntentInput,
  ): Promise<StrongFlowGitHubPublicationJournal> {
    let operations: readonly StrongFlowGitHubProviderOperation[]
    try {
      operations = Object.freeze(input.operations.map(entry => (
        parseStrongFlowGitHubProviderOperation(entry)
      )))
    } catch (error) {
      return journalError('INVALID_JOURNAL_INPUT', 'publication operations are invalid', error)
    }
    const unsigned = intentWithoutDigest({
      schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_JOURNAL_SCHEMA_VERSION,
      protocol: STRONGFLOW_GITHUB_PUBLICATION_INTENT_PROTOCOL,
      runId: runIdentity(input),
      providerIdempotencyKey: input.providerIdempotencyKey,
      reviewPackageId: input.reviewPackageId,
      deliveryId: input.deliveryId,
      deliverySpecId: input.deliverySpecId,
      deliverySpecRevision: input.deliverySpecRevision,
      candidateRef: input.candidateRef,
      deliveryVerdictId: input.deliveryVerdictId,
      publicationSetSha256: input.publicationSetSha256,
      operations,
      createdAtMillis: input.createdAtMillis,
    })
    let intent: StrongFlowGitHubPublicationIntent
    try {
      intent = parseIntent({ ...unsigned, digest: digest(unsigned) })
    } catch (error) {
      return journalError('INVALID_JOURNAL_INPUT', 'publication intent is invalid', error)
    }
    const home = resolve(input.home)
    const directory = runDirectory(home, intent)
    if (await pathExists(directory)) {
      const opened = await StrongFlowGitHubPublicationJournal.open(home, intent)
      if (!equal(opened.intent, intent)) {
        return journalError('JOURNAL_CONFLICT', 'publication run intent changed')
      }
      return opened
    }
    const parent = join(journalRoot(home), createHash('sha256')
      .update(intent.providerIdempotencyKey).digest('hex'))
    await mkdir(parent, { recursive: true, mode: 0o700 })
    const temporary = join(parent, `.creating-${randomUUID()}`)
    try {
      await mkdir(temporary, { mode: 0o700 })
      await mkdir(join(temporary, 'events'), { mode: 0o700 })
      await writeDurable(join(temporary, 'intent.json'), `${JSON.stringify(intent, null, 2)}\n`)
      await syncDirectory(join(temporary, 'events'))
      await syncDirectory(temporary)
      await rename(temporary, directory)
      await syncDirectory(parent)
      return new StrongFlowGitHubPublicationJournal(home, directory, intent)
    } catch (error) {
      await rm(temporary, { recursive: true, force: true })
      if (await pathExists(directory)) {
        const opened = await StrongFlowGitHubPublicationJournal.open(home, intent)
        if (!equal(opened.intent, intent)) {
          return journalError('JOURNAL_CONFLICT', 'publication run intent changed')
        }
        return opened
      }
      return journalError('JOURNAL_IO_ERROR', 'publication intent could not be stored', error)
    }
  }

  static async open(
    homeInput: string,
    expectedIntent: StrongFlowGitHubPublicationIntent,
  ): Promise<StrongFlowGitHubPublicationJournal> {
    const home = resolve(homeInput)
    const directory = runDirectory(home, expectedIntent)
    try {
      if (!(await lstat(directory)).isDirectory()) throw new Error('run path is not a directory')
      const text = await readFile(join(directory, 'intent.json'), 'utf8')
      const intent = parseIntent(JSON.parse(text) as unknown)
      if (basename(directory) !== createHash('sha256')
        .update(intent.reviewPackageId).digest('hex')
        || !equal(intent, expectedIntent)) {
        return journalError('JOURNAL_CONFLICT', 'publication run identity changed')
      }
      const journal = new StrongFlowGitHubPublicationJournal(home, directory, intent)
      await journal.#readUnlocked()
      return journal
    } catch (error) {
      if (error instanceof StrongFlowGitHubPublicationJournalError) throw error
      return journalError('JOURNAL_CORRUPT', 'publication journal could not be opened', error)
    }
  }

  async read(): Promise<StrongFlowGitHubPublicationJournalProjection> {
    await this.#tail
    return this.#readUnlocked()
  }

  async append(
    input: AppendStrongFlowGitHubPublicationEventInput,
  ): Promise<StrongFlowGitHubPublicationJournalProjection> {
    return this.#serialize(async () => {
      const operation = parseStrongFlowGitHubProviderOperation(input.operation)
      if (!this.intent.operations.some(expected => equal(expected, operation))) {
        return journalError('JOURNAL_CONFLICT', 'publication event names another operation')
      }
      const event = parseEvent(materializeEvent(this.intent.runId, { ...input, operation }))
      const temporary = join(this.eventsDirectory, `.pending-${randomUUID()}.json`)
      const published = join(this.eventsDirectory, `${event.eventId}.json`)
      try {
        await writeDurable(temporary, `${JSON.stringify(event)}\n`)
        await link(temporary, published)
        await syncDirectory(this.eventsDirectory)
      } catch (error) {
        if (errorCode(error) !== 'EEXIST') {
          await rm(temporary, { force: true })
          return journalError('JOURNAL_IO_ERROR', 'publication event could not be stored', error)
        }
        const existing = parseEvent(JSON.parse(await readFile(published, 'utf8')) as unknown)
        if (!equal(existing, event)) {
          await rm(temporary, { force: true })
          return journalError('JOURNAL_CONFLICT', 'publication event identity collided')
        }
      }
      await rm(temporary, { force: true })
      return this.#readUnlocked()
    })
  }

  async #readUnlocked(): Promise<StrongFlowGitHubPublicationJournalProjection> {
    try {
      const intent = parseIntent(JSON.parse(await readFile(this.intentPath, 'utf8')) as unknown)
      if (!equal(intent, this.intent)) {
        return journalError('JOURNAL_CONFLICT', 'publication intent changed on disk')
      }
      const entries = await readdir(this.eventsDirectory, { withFileTypes: true })
      const events: StrongFlowGitHubPublicationEvent[] = []
      for (const entry of entries) {
        if (PENDING_FILE_PATTERN.test(entry.name)) continue
        const match = EVENT_FILE_PATTERN.exec(entry.name)
        if (!entry.isFile() || match === null) {
          return journalError('JOURNAL_CORRUPT', 'publication journal has an unexpected entry')
        }
        const event = parseEvent(
          JSON.parse(await readFile(join(this.eventsDirectory, entry.name), 'utf8')) as unknown,
        )
        if (event.eventId !== match[1]
          || event.runId !== intent.runId
          || !intent.operations.some(operation => (
            operation.operationKey === event.operationKey
            && operation.requestSha256 === event.requestSha256
          ))) {
          return journalError('JOURNAL_CORRUPT', 'publication event relationship changed')
        }
        events.push(event)
      }
      events.sort((left, right) => (
        left.recordedAtMillis - right.recordedAtMillis || left.eventId.localeCompare(right.eventId)
      ))
      return projection(intent, Object.freeze(events))
    } catch (error) {
      if (error instanceof StrongFlowGitHubPublicationJournalError) throw error
      return journalError('JOURNAL_CORRUPT', 'publication journal is corrupt', error)
    }
  }

  #serialize<Result>(operation: () => Promise<Result>): Promise<Result> {
    const current = this.#tail.then(operation, operation)
    this.#tail = current.then(() => {}, () => {})
    return current
  }
}
