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
import { basename, dirname, join, resolve } from 'node:path'
import { isDeepStrictEqual } from 'node:util'

import {
  AttemptId,
  CandidateId,
  GitDiffId,
  JobId,
  KernelSessionId,
  StageRunId,
  StrongFlowHandoffId,
  STRONGFLOW_ARTIFACT_KINDS,
  STRONGFLOW_ROLE_IDS,
  parseStrongFlowArtifact,
  parseStrongFlowArtifactKernelEventInterval,
  parseStrongFlowCandidateIdentity,
  parseStrongFlowHandoffManifest,
  type AttemptId as AttemptIdentifier,
  type CandidateId as CandidateIdentifier,
  type GitDiffId as GitDiffIdentifier,
  type JobId as JobIdentifier,
  type KernelSessionId as KernelSessionIdentifier,
  type StageRunId as StageRunIdentifier,
  type StrongFlowArtifact,
  type StrongFlowArtifactKernelEventInterval,
  type StrongFlowArtifactKind,
  type StrongFlowCandidateIdentity,
  type StrongFlowHandoffManifest,
  type StrongFlowHandoffTarget,
  type StrongFlowRoleId,
} from '@winwincode/contracts'

export const STRONGFLOW_ARTIFACT_STORE_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_ARTIFACT_STORE_MAX_BLOB_BYTES = 64 * 1024 * 1024
export const STRONGFLOW_ARTIFACT_STORE_MAX_LIST_LIMIT = 1_000

const ARTIFACT_MEDIA_TYPE = 'application/vnd.winwincode.strongflow-artifact+json'
const HANDOFF_MEDIA_TYPE = 'application/vnd.winwincode.strongflow-handoff+json'
const PORTABLE_IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/-]{0,199}$/u
const DECIMAL_SEQUENCE_PATTERN = /^(?:0|[1-9][0-9]*)$/u
const SHA256_PATTERN = /^[0-9a-f]{64}$/u
const BLOB_ID_PATTERN = /^sha256-[0-9a-f]{64}$/u
const RECORD_HASH_PATTERN = /^record-sha256-[0-9a-f]{64}$/u
const JOB_DIRECTORY_PATTERN = /^[0-9a-f]{64}$/u
const RECORD_FILE_PATTERN = /^([1-9][0-9]*)\.json$/u
const CREATING_DIRECTORY_PATTERN = /^\.creating-[0-9a-f]{64}-[0-9a-f-]+$/u
const PENDING_RECORD_PATTERN = /^\.pending-[1-9][0-9]*-[0-9a-f-]+\.json$/u
const PENDING_BLOB_PATTERN = /^\.pending-[0-9a-f]{64}-[0-9a-f-]+\.blob$/u
const MEDIA_TYPE_PATTERN = /^[a-z0-9!#$&^_.+-]+\/[a-z0-9!#$&^_.+-]+(?:; charset=utf-8)?$/u

declare const artifactStoreIdentifierBrand: unique symbol

type ArtifactStoreIdentifier<Name extends string> = string & {
  readonly [artifactStoreIdentifierBrand]: Name
}

export type StrongFlowBlobId = ArtifactStoreIdentifier<'StrongFlowBlobId'>
export type StrongFlowArtifactStoreRecordId = ArtifactStoreIdentifier<
  'StrongFlowArtifactStoreRecordId'
>
export type StrongFlowArtifactStoreRecordHash = ArtifactStoreIdentifier<
  'StrongFlowArtifactStoreRecordHash'
>

export type StrongFlowArtifactStoreErrorCode =
  | 'INVALID_STORE_OPTIONS'
  | 'JOB_ALREADY_EXISTS'
  | 'JOB_NOT_FOUND'
  | 'JOB_ID_MISMATCH'
  | 'ENTRY_INVALID'
  | 'IDENTITY_CONFLICT'
  | 'RECORD_NOT_FOUND'
  | 'CONTENT_MISSING'
  | 'CONTENT_DIGEST_MISMATCH'
  | 'CONTENT_TOO_LARGE'
  | 'STORE_CORRUPT'
  | 'STORE_IO_ERROR'

/** Stable failure from the immutable artifact, handoff, and evidence persistence module. */
export class StrongFlowArtifactStoreError extends Error {
  readonly code: StrongFlowArtifactStoreErrorCode

  constructor(
    code: StrongFlowArtifactStoreErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowArtifactStoreError'
    this.code = code
  }
}

export const STRONGFLOW_EVIDENCE_KINDS = Object.freeze([
  'command',
  'test',
  'diff',
  'render',
  'log',
  'other',
] as const)

export type StrongFlowEvidenceKind = typeof STRONGFLOW_EVIDENCE_KINDS[number]
export type StrongFlowEvidenceTrust = 'trusted-direct-command' | 'model-observation'

export interface StrongFlowArtifactStoreManifest {
  readonly schemaVersion: typeof STRONGFLOW_ARTIFACT_STORE_SCHEMA_VERSION
  readonly jobId: JobIdentifier
  readonly createdAtMillis: number
}

export interface StrongFlowStoredRoleProducer {
  readonly kind: 'role'
  readonly roleId: StrongFlowRoleId
  readonly stageRunId: StageRunIdentifier
  readonly attemptId: AttemptIdentifier
  readonly eventInterval: StrongFlowArtifactKernelEventInterval
}

export type StrongFlowStoredProducer =
  | StrongFlowStoredRoleProducer
  | {
    readonly kind: 'human'
    readonly actorId: string
    readonly channel: 'local-ui' | 'cli'
  }
  | {
    readonly kind: 'system'
    readonly actorId: string
  }

export type StrongFlowStoredCandidateReference =
  | {
    readonly kind: 'complete'
    readonly identity: StrongFlowCandidateIdentity
  }
  | {
    readonly kind: 'diff'
    readonly candidateId: CandidateIdentifier
    readonly diffId: GitDiffIdentifier
  }

export interface StrongFlowStoredBlobReference {
  readonly blobId: StrongFlowBlobId
  readonly byteLength: number
  readonly mediaType: string
}

export interface StrongFlowStoredArtifactIdentity {
  readonly kind: 'artifact'
  readonly artifactKind: StrongFlowArtifactKind
  readonly artifactId: string
}

export interface StrongFlowStoredEvidenceIdentity {
  readonly kind: 'evidence'
  readonly evidenceId: string
  readonly evidenceKind: StrongFlowEvidenceKind
  readonly trust: StrongFlowEvidenceTrust
  readonly sourceArtifact: StrongFlowStoredArtifactIdentity | null
  readonly command: {
    readonly commandId: string
    readonly exitCode: number
  } | null
}

export interface StrongFlowStoredHandoffIdentity {
  readonly kind: 'handoff'
  readonly handoffId: string
  readonly target: StrongFlowHandoffTarget
}

interface StrongFlowArtifactStoreRecordBase {
  readonly schemaVersion: typeof STRONGFLOW_ARTIFACT_STORE_SCHEMA_VERSION
  readonly recordId: StrongFlowArtifactStoreRecordId
  readonly jobId: JobIdentifier
  readonly sequence: string
  readonly blob: StrongFlowStoredBlobReference
  readonly producer: StrongFlowStoredProducer
  readonly candidate: StrongFlowStoredCandidateReference | null
  readonly createdAtMillis: number
  readonly previousRecordHash: StrongFlowArtifactStoreRecordHash | null
  readonly recordHash: StrongFlowArtifactStoreRecordHash
}

export interface StrongFlowArtifactStoreArtifactRecord
  extends StrongFlowArtifactStoreRecordBase {
  readonly entryKind: 'artifact'
  readonly identity: StrongFlowStoredArtifactIdentity
}

export interface StrongFlowArtifactStoreEvidenceRecord
  extends StrongFlowArtifactStoreRecordBase {
  readonly entryKind: 'direct-command-evidence' | 'model-observation'
  readonly identity: StrongFlowStoredEvidenceIdentity
  readonly producer: StrongFlowStoredRoleProducer
}

export interface StrongFlowArtifactStoreHandoffRecord
  extends StrongFlowArtifactStoreRecordBase {
  readonly entryKind: 'handoff'
  readonly identity: StrongFlowStoredHandoffIdentity
  readonly producer: Extract<StrongFlowStoredProducer, { readonly kind: 'system' }>
}

export type StrongFlowArtifactStoreRecord =
  | StrongFlowArtifactStoreArtifactRecord
  | StrongFlowArtifactStoreEvidenceRecord
  | StrongFlowArtifactStoreHandoffRecord

export interface CreateStrongFlowArtifactStoreOptions {
  readonly home: string
  readonly jobId: JobIdentifier
  readonly createdAtMillis: number
}

export interface StrongFlowEvidenceProducerInput {
  readonly roleId: StrongFlowRoleId
  readonly stageRunId: StageRunIdentifier
  readonly attemptId: AttemptIdentifier
  readonly eventInterval: StrongFlowArtifactKernelEventInterval
}

interface StrongFlowEvidenceInputBase {
  readonly jobId: JobIdentifier
  readonly evidenceId: string
  readonly evidenceKind: StrongFlowEvidenceKind
  readonly content: Uint8Array
  readonly mediaType: string
  readonly producer: StrongFlowEvidenceProducerInput
  readonly candidate: StrongFlowCandidateIdentity | null
  readonly createdAtMillis: number
}

export interface PublishStrongFlowDirectEvidenceInput extends StrongFlowEvidenceInputBase {
  readonly command: {
    readonly commandId: string
    readonly exitCode: number
  } | null
}

export interface PublishStrongFlowModelObservationInput extends StrongFlowEvidenceInputBase {
  readonly sourceArtifact: StrongFlowStoredArtifactIdentity
}

export interface StrongFlowArtifactStorePublishReceipt {
  readonly outcome: 'published' | 'already-published'
  readonly blobReused: boolean
  readonly record: StrongFlowArtifactStoreRecord
}

export interface StrongFlowStoredArtifactContent {
  readonly record: StrongFlowArtifactStoreArtifactRecord
  readonly artifact: StrongFlowArtifact
}

export interface StrongFlowStoredEvidenceContent {
  readonly record: StrongFlowArtifactStoreEvidenceRecord
  /** A new caller-owned byte copy is returned for each read. */
  readonly content: Uint8Array
}

export interface StrongFlowStoredHandoffContent {
  readonly record: StrongFlowArtifactStoreHandoffRecord
  readonly handoff: StrongFlowHandoffManifest
}

export type StrongFlowArtifactStoreReadResult =
  | StrongFlowStoredArtifactContent
  | StrongFlowStoredEvidenceContent
  | StrongFlowStoredHandoffContent

export interface StrongFlowArtifactStoreListQuery {
  readonly limit: number
  readonly afterSequence?: string
  readonly attemptId?: AttemptIdentifier
  readonly entryKinds?: readonly StrongFlowArtifactStoreRecord['entryKind'][]
}

export interface StrongFlowArtifactStoreListResult {
  readonly records: readonly StrongFlowArtifactStoreRecord[]
  readonly nextAfterSequence: string | null
}

function storeError(
  code: StrongFlowArtifactStoreErrorCode,
  message: string,
  options?: ErrorOptions,
): never {
  throw new StrongFlowArtifactStoreError(code, message, options)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (!isRecord(value)) storeError('ENTRY_INVALID', `${label} must be an object`)
  return value
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
  ) storeError('ENTRY_INVALID', `${label} has an unexpected shape`)
}

function errorCode(error: unknown): string | undefined {
  if (typeof error !== 'object' || error === null || !('code' in error)) return undefined
  return typeof error.code === 'string' ? error.code : undefined
}

function validateHome(value: unknown): string {
  if (typeof value !== 'string' || value.length === 0) {
    storeError('INVALID_STORE_OPTIONS', 'artifact store home must be a non-empty path')
  }
  return resolve(value)
}

function portableIdentifier(value: unknown, label: string): string {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    storeError('ENTRY_INVALID', `${label} is not a portable identifier`)
  }
  return value
}

function nonNegativeInteger(value: unknown, label: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0) {
    storeError('ENTRY_INVALID', `${label} must be a non-negative safe integer`)
  }
  return Number(value)
}

function canonicalSequence(value: unknown, label: string): string {
  if (typeof value !== 'string' || !DECIMAL_SEQUENCE_PATTERN.test(value)) {
    storeError('ENTRY_INVALID', `${label} must be a canonical decimal string`)
  }
  return value
}

function mediaType(value: unknown, label: string): string {
  if (typeof value !== 'string' || value.length > 200 || !MEDIA_TYPE_PATTERN.test(value)) {
    storeError('ENTRY_INVALID', `${label} is not a supported media type`)
  }
  return value
}

function immutable<Value>(value: Value): Value {
  if (Array.isArray(value)) {
    for (const entry of value) immutable(entry)
    return Object.freeze(value)
  }
  if (isRecord(value)) {
    for (const entry of Object.values(value)) immutable(entry)
    return Object.freeze(value) as Value
  }
  return value
}

function canonicalJson(value: unknown): string {
  if (value === null || typeof value === 'boolean' || typeof value === 'string') {
    return JSON.stringify(value)
  }
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) storeError('ENTRY_INVALID', 'record contains a non-finite number')
    return JSON.stringify(value)
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`
  if (!isRecord(value)) storeError('ENTRY_INVALID', 'record contains non-JSON data')
  return `{${Object.keys(value).sort().map(key => (
    `${JSON.stringify(key)}:${canonicalJson(value[key])}`
  )).join(',')}}`
}

function sha256(value: Uint8Array | string): string {
  return createHash('sha256').update(value).digest('hex')
}

export function StrongFlowBlobId(value: string): StrongFlowBlobId {
  if (!BLOB_ID_PATTERN.test(value)) {
    storeError('ENTRY_INVALID', 'blob id is invalid')
  }
  return value as StrongFlowBlobId
}

function recordHash(value: Omit<StrongFlowArtifactStoreRecord, 'recordHash'>): StrongFlowArtifactStoreRecordHash {
  return `record-sha256-${sha256(canonicalJson(value))}` as StrongFlowArtifactStoreRecordHash
}

function artifactStoreRecordId(
  jobId: JobIdentifier,
  sequence: string,
): StrongFlowArtifactStoreRecordId {
  return `${jobId}@artifact-record:${sequence}` as StrongFlowArtifactStoreRecordId
}

function storeRoot(home: string): string {
  return join(home, 'strongflow-artifacts')
}

function blobsRoot(home: string): string {
  return join(storeRoot(home), 'blobs', 'sha256')
}

function jobsRoot(home: string): string {
  return join(storeRoot(home), 'jobs')
}

function jobKey(jobId: string): string {
  return sha256(jobId)
}

function jobDirectory(home: string, jobId: string): string {
  return join(jobsRoot(home), jobKey(jobId))
}

function blobPath(root: string, blobId: StrongFlowBlobId): string {
  const digest = blobId.slice('sha256-'.length)
  return join(root, digest.slice(0, 2), `${digest}.blob`)
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

async function writeNewFileDurable(path: string, content: Uint8Array | string): Promise<void> {
  const handle = await open(path, 'wx', 0o600)
  try {
    await handle.writeFile(content)
    await handle.sync()
  } finally {
    await handle.close()
  }
}

function parseManifest(value: unknown): StrongFlowArtifactStoreManifest {
  const input = record(value, 'artifact store manifest')
  exactKeys(input, ['schemaVersion', 'jobId', 'createdAtMillis'], [], 'artifact store manifest')
  if (input.schemaVersion !== STRONGFLOW_ARTIFACT_STORE_SCHEMA_VERSION) {
    storeError('STORE_CORRUPT', 'artifact store manifest version is unsupported')
  }
  let jobId: JobIdentifier
  try {
    jobId = JobId(String(input.jobId))
  } catch (error) {
    storeError('STORE_CORRUPT', 'artifact store manifest job id is invalid', { cause: error })
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_ARTIFACT_STORE_SCHEMA_VERSION,
    jobId,
    createdAtMillis: nonNegativeInteger(input.createdAtMillis, 'manifest.createdAtMillis'),
  })
}

async function loadManifest(path: string): Promise<StrongFlowArtifactStoreManifest> {
  const text = await readFile(path, 'utf8')
  return parseManifest(JSON.parse(text) as unknown)
}

function parseStoredProducer(value: unknown, label: string): StrongFlowStoredProducer {
  const input = record(value, label)
  if (input.kind === 'role') {
    exactKeys(
      input,
      ['kind', 'roleId', 'stageRunId', 'attemptId', 'eventInterval'],
      [],
      label,
    )
    if (typeof input.roleId !== 'string' || !STRONGFLOW_ROLE_IDS.includes(
      input.roleId as StrongFlowRoleId,
    )) storeError('ENTRY_INVALID', `${label}.roleId is unsupported`)
    const interval = parseStrongFlowArtifactKernelEventInterval(
      input.eventInterval,
      `${label}.eventInterval`,
    )
    if (interval === null) storeError('ENTRY_INVALID', `${label}.eventInterval is required`)
    try {
      return Object.freeze({
        kind: 'role',
        roleId: input.roleId as StrongFlowRoleId,
        stageRunId: StageRunId(String(input.stageRunId)),
        attemptId: AttemptId(String(input.attemptId)),
        eventInterval: interval,
      })
    } catch (error) {
      storeError('ENTRY_INVALID', `${label} contains an invalid role identity`, { cause: error })
    }
  }
  if (input.kind === 'human') {
    exactKeys(input, ['kind', 'actorId', 'channel'], [], label)
    if (input.channel !== 'local-ui' && input.channel !== 'cli') {
      storeError('ENTRY_INVALID', `${label}.channel is unsupported`)
    }
    return Object.freeze({
      kind: 'human',
      actorId: portableIdentifier(input.actorId, `${label}.actorId`),
      channel: input.channel,
    })
  }
  if (input.kind === 'system') {
    exactKeys(input, ['kind', 'actorId'], [], label)
    return Object.freeze({
      kind: 'system',
      actorId: portableIdentifier(input.actorId, `${label}.actorId`),
    })
  }
  storeError('ENTRY_INVALID', `${label}.kind is unsupported`)
}

function parseCandidateReference(
  value: unknown,
  label: string,
): StrongFlowStoredCandidateReference | null {
  if (value === null) return null
  const input = record(value, label)
  if (input.kind === 'complete') {
    exactKeys(input, ['kind', 'identity'], [], label)
    return Object.freeze({
      kind: 'complete',
      identity: parseStrongFlowCandidateIdentity(input.identity, `${label}.identity`),
    })
  }
  if (input.kind === 'diff') {
    exactKeys(input, ['kind', 'candidateId', 'diffId'], [], label)
    try {
      return Object.freeze({
        kind: 'diff',
        candidateId: CandidateId(String(input.candidateId)),
        diffId: GitDiffId(String(input.diffId)),
      })
    } catch (error) {
      storeError('ENTRY_INVALID', `${label} contains an invalid candidate link`, { cause: error })
    }
  }
  storeError('ENTRY_INVALID', `${label}.kind is unsupported`)
}

function parseBlobReference(value: unknown, label: string): StrongFlowStoredBlobReference {
  const input = record(value, label)
  exactKeys(input, ['blobId', 'byteLength', 'mediaType'], [], label)
  return Object.freeze({
    blobId: StrongFlowBlobId(String(input.blobId)),
    byteLength: nonNegativeInteger(input.byteLength, `${label}.byteLength`),
    mediaType: mediaType(input.mediaType, `${label}.mediaType`),
  })
}

function parseArtifactIdentity(value: unknown, label: string): StrongFlowStoredArtifactIdentity {
  const input = record(value, label)
  exactKeys(input, ['kind', 'artifactKind', 'artifactId'], [], label)
  if (input.kind !== 'artifact') storeError('ENTRY_INVALID', `${label}.kind is invalid`)
  if (typeof input.artifactKind !== 'string' || !STRONGFLOW_ARTIFACT_KINDS.includes(
    input.artifactKind as StrongFlowArtifactKind,
  )) storeError('ENTRY_INVALID', `${label}.artifactKind is unsupported`)
  return Object.freeze({
    kind: 'artifact',
    artifactKind: input.artifactKind as StrongFlowArtifactKind,
    artifactId: portableIdentifier(input.artifactId, `${label}.artifactId`),
  })
}

function parseHandoffIdentity(value: unknown, label: string): StrongFlowStoredHandoffIdentity {
  const input = record(value, label)
  exactKeys(input, ['kind', 'handoffId', 'target'], [], label)
  if (input.kind !== 'handoff') storeError('ENTRY_INVALID', `${label}.kind is invalid`)
  const targetInput = record(input.target, `${label}.target`)
  let target: StrongFlowHandoffTarget
  if (targetInput.kind === 'human-review') {
    exactKeys(targetInput, ['kind'], [], `${label}.target`)
    target = Object.freeze({ kind: 'human-review' })
  } else {
    exactKeys(
      targetInput,
      ['kind', 'roleId', 'stageRunId', 'attemptId'],
      [],
      `${label}.target`,
    )
    if (targetInput.kind !== 'role'
      || typeof targetInput.roleId !== 'string'
      || !STRONGFLOW_ROLE_IDS.includes(targetInput.roleId as StrongFlowRoleId)) {
      storeError('ENTRY_INVALID', `${label}.target is invalid`)
    }
    try {
      target = Object.freeze({
        kind: 'role',
        roleId: targetInput.roleId as StrongFlowRoleId,
        stageRunId: StageRunId(String(targetInput.stageRunId)),
        attemptId: AttemptId(String(targetInput.attemptId)),
      })
    } catch (error) {
      storeError('ENTRY_INVALID', `${label}.target identity is invalid`, { cause: error })
    }
  }
  return Object.freeze({
    kind: 'handoff',
    handoffId: StrongFlowHandoffId(String(input.handoffId)),
    target,
  })
}

function parseCommand(
  value: unknown,
  label: string,
): StrongFlowStoredEvidenceIdentity['command'] {
  if (value === null) return null
  const input = record(value, label)
  exactKeys(input, ['commandId', 'exitCode'], [], label)
  if (!Number.isSafeInteger(input.exitCode)) {
    storeError('ENTRY_INVALID', `${label}.exitCode must be a safe integer`)
  }
  return Object.freeze({
    commandId: portableIdentifier(input.commandId, `${label}.commandId`),
    exitCode: Number(input.exitCode),
  })
}

function parseEvidenceIdentity(value: unknown, label: string): StrongFlowStoredEvidenceIdentity {
  const input = record(value, label)
  exactKeys(input, [
    'kind',
    'evidenceId',
    'evidenceKind',
    'trust',
    'sourceArtifact',
    'command',
  ], [], label)
  if (input.kind !== 'evidence') storeError('ENTRY_INVALID', `${label}.kind is invalid`)
  if (typeof input.evidenceKind !== 'string' || !STRONGFLOW_EVIDENCE_KINDS.includes(
    input.evidenceKind as StrongFlowEvidenceKind,
  )) storeError('ENTRY_INVALID', `${label}.evidenceKind is unsupported`)
  if (input.trust !== 'trusted-direct-command' && input.trust !== 'model-observation') {
    storeError('ENTRY_INVALID', `${label}.trust is unsupported`)
  }
  const sourceArtifact = input.sourceArtifact === null
    ? null
    : parseArtifactIdentity(input.sourceArtifact, `${label}.sourceArtifact`)
  const command = parseCommand(input.command, `${label}.command`)
  if (input.trust === 'trusted-direct-command') {
    if (sourceArtifact !== null) {
      storeError('ENTRY_INVALID', 'direct evidence cannot claim a model source artifact')
    }
    if (['command', 'test'].includes(input.evidenceKind as string) && command === null) {
      storeError('ENTRY_INVALID', 'command and test evidence require command metadata')
    }
  } else if (sourceArtifact === null || command !== null) {
    storeError('ENTRY_INVALID', 'model observations require one source artifact and no command')
  }
  return Object.freeze({
    kind: 'evidence',
    evidenceId: portableIdentifier(input.evidenceId, `${label}.evidenceId`),
    evidenceKind: input.evidenceKind as StrongFlowEvidenceKind,
    trust: input.trust,
    sourceArtifact,
    command,
  })
}

function recordWithoutHash(
  input: Record<string, unknown>,
  identity:
    | StrongFlowStoredArtifactIdentity
    | StrongFlowStoredEvidenceIdentity
    | StrongFlowStoredHandoffIdentity,
  producer: StrongFlowStoredProducer,
  candidate: StrongFlowStoredCandidateReference | null,
  blob: StrongFlowStoredBlobReference,
): Omit<StrongFlowArtifactStoreRecord, 'recordHash'> {
  return {
    schemaVersion: STRONGFLOW_ARTIFACT_STORE_SCHEMA_VERSION,
    recordId: String(input.recordId) as StrongFlowArtifactStoreRecordId,
    jobId: JobId(String(input.jobId)),
    sequence: String(input.sequence),
    entryKind: input.entryKind as StrongFlowArtifactStoreRecord['entryKind'],
    identity,
    blob,
    producer,
    candidate,
    createdAtMillis: Number(input.createdAtMillis),
    previousRecordHash: input.previousRecordHash as StrongFlowArtifactStoreRecordHash | null,
  } as Omit<StrongFlowArtifactStoreRecord, 'recordHash'>
}

function parseStoreRecord(value: unknown): StrongFlowArtifactStoreRecord {
  const input = record(value, 'artifact store record')
  exactKeys(input, [
    'schemaVersion',
    'recordId',
    'jobId',
    'sequence',
    'entryKind',
    'identity',
    'blob',
    'producer',
    'candidate',
    'createdAtMillis',
    'previousRecordHash',
    'recordHash',
  ], [], 'artifact store record')
  if (input.schemaVersion !== STRONGFLOW_ARTIFACT_STORE_SCHEMA_VERSION) {
    storeError('ENTRY_INVALID', 'artifact store record version is unsupported')
  }
  let jobId: JobIdentifier
  try {
    jobId = JobId(String(input.jobId))
  } catch (error) {
    storeError('ENTRY_INVALID', 'artifact store record job id is invalid', { cause: error })
  }
  const sequence = canonicalSequence(input.sequence, 'record.sequence')
  if (sequence === '0' || input.recordId !== artifactStoreRecordId(jobId, sequence)) {
    storeError('ENTRY_INVALID', 'artifact store record identity is invalid')
  }
  if (![
    'artifact',
    'direct-command-evidence',
    'model-observation',
    'handoff',
  ].includes(String(input.entryKind))) {
    storeError('ENTRY_INVALID', 'artifact store record kind is unsupported')
  }
  const producer = parseStoredProducer(input.producer, 'record.producer')
  const candidate = parseCandidateReference(input.candidate, 'record.candidate')
  const blob = parseBlobReference(input.blob, 'record.blob')
  const identity = input.entryKind === 'artifact'
    ? parseArtifactIdentity(input.identity, 'record.identity')
    : input.entryKind === 'handoff'
      ? parseHandoffIdentity(input.identity, 'record.identity')
      : parseEvidenceIdentity(input.identity, 'record.identity')
  if (input.entryKind === 'artifact') {
    if (identity.kind !== 'artifact') storeError('ENTRY_INVALID', 'artifact identity is invalid')
  } else if (input.entryKind === 'handoff') {
    if (identity.kind !== 'handoff' || producer.kind !== 'system') {
      storeError('ENTRY_INVALID', 'handoff record identity or producer is invalid')
    }
  } else {
    if (identity.kind !== 'evidence' || producer.kind !== 'role') {
      storeError('ENTRY_INVALID', 'evidence record identity or producer is invalid')
    }
    if (
      (input.entryKind === 'direct-command-evidence')
      !== (identity.trust === 'trusted-direct-command')
    ) storeError('ENTRY_INVALID', 'evidence trust does not match its record kind')
  }
  nonNegativeInteger(input.createdAtMillis, 'record.createdAtMillis')
  if (
    input.previousRecordHash !== null
    && (typeof input.previousRecordHash !== 'string'
      || !RECORD_HASH_PATTERN.test(input.previousRecordHash))
  ) storeError('ENTRY_INVALID', 'record.previousRecordHash is invalid')
  if (typeof input.recordHash !== 'string' || !RECORD_HASH_PATTERN.test(input.recordHash)) {
    storeError('ENTRY_INVALID', 'record.recordHash is invalid')
  }
  const withoutHash = recordWithoutHash(input, identity, producer, candidate, blob)
  if (withoutHash.jobId !== jobId || withoutHash.sequence !== sequence) {
    storeError('ENTRY_INVALID', 'artifact store record changed during parsing')
  }
  if (recordHash(withoutHash) !== input.recordHash) {
    storeError('ENTRY_INVALID', 'artifact store record hash does not match its content')
  }
  return immutable({ ...withoutHash, recordHash: input.recordHash }) as StrongFlowArtifactStoreRecord
}

function nextSequence(sequence: string | undefined): string {
  return sequence === undefined ? '1' : (BigInt(sequence) + 1n).toString()
}

function compareSequence(left: string, right: string): number {
  const leftValue = BigInt(left)
  const rightValue = BigInt(right)
  return leftValue < rightValue ? -1 : leftValue > rightValue ? 1 : 0
}

function artifactProducer(artifact: StrongFlowArtifact): StrongFlowStoredProducer {
  if (artifact.producer.kind === 'role') {
    if (artifact.kernelEventInterval === null) {
      storeError('ENTRY_INVALID', 'role artifact has no kernel event interval')
    }
    return Object.freeze({
      kind: 'role',
      roleId: artifact.producer.roleId,
      stageRunId: artifact.producer.stageRunId,
      attemptId: artifact.producer.attemptId,
      eventInterval: artifact.kernelEventInterval,
    })
  }
  return artifact.producer
}

function artifactCandidate(
  artifact: StrongFlowArtifact,
): StrongFlowStoredCandidateReference | null {
  switch (artifact.artifactKind) {
    case 'PATCH_MANIFEST':
    case 'REVIEW_REPORT':
    case 'VERIFICATION_REPORT':
    case 'REMEDIATION_REQUEST':
    case 'REMEDIATION_REPORT':
    case 'DELIVERY_RECEIPT':
      return Object.freeze({ kind: 'complete', identity: artifact.payload.candidate })
    case 'EXECUTION_CHANGE_ANNOTATION':
      return Object.freeze({
        kind: 'diff',
        candidateId: artifact.payload.candidateId,
        diffId: artifact.payload.diffId,
      })
    default:
      return null
  }
}

function parseEvidenceProducer(value: unknown): StrongFlowStoredRoleProducer {
  const input = record(value, 'evidence.producer')
  exactKeys(input, ['roleId', 'stageRunId', 'attemptId', 'eventInterval'], [], 'evidence.producer')
  return parseStoredProducer({ kind: 'role', ...input }, 'evidence.producer') as StrongFlowStoredRoleProducer
}

interface ParsedEvidenceInput {
  readonly jobId: JobIdentifier
  readonly identity: StrongFlowStoredEvidenceIdentity
  readonly content: Uint8Array
  readonly mediaType: string
  readonly producer: StrongFlowStoredRoleProducer
  readonly candidate: StrongFlowStoredCandidateReference | null
  readonly createdAtMillis: number
}

function parseEvidenceInput(
  value: unknown,
  trust: StrongFlowEvidenceTrust,
): ParsedEvidenceInput {
  const input = record(value, 'evidence')
  const direct = trust === 'trusted-direct-command'
  exactKeys(input, [
    'jobId',
    'evidenceId',
    'evidenceKind',
    'content',
    'mediaType',
    'producer',
    'candidate',
    'createdAtMillis',
    direct ? 'command' : 'sourceArtifact',
  ], [], 'evidence')
  let jobId: JobIdentifier
  try {
    jobId = JobId(String(input.jobId))
  } catch (error) {
    storeError('ENTRY_INVALID', 'evidence job id is invalid', { cause: error })
  }
  if (!(input.content instanceof Uint8Array)) {
    storeError('ENTRY_INVALID', 'evidence content must be a byte array')
  }
  if (input.content.byteLength === 0) {
    storeError('ENTRY_INVALID', 'evidence content must not be empty')
  }
  if (input.content.byteLength > STRONGFLOW_ARTIFACT_STORE_MAX_BLOB_BYTES) {
    storeError('CONTENT_TOO_LARGE', 'evidence content exceeds the store limit')
  }
  const evidenceKind = typeof input.evidenceKind === 'string'
    && STRONGFLOW_EVIDENCE_KINDS.includes(input.evidenceKind as StrongFlowEvidenceKind)
    ? input.evidenceKind as StrongFlowEvidenceKind
    : storeError('ENTRY_INVALID', 'evidence kind is unsupported')
  const identity = parseEvidenceIdentity({
    kind: 'evidence',
    evidenceId: input.evidenceId,
    evidenceKind,
    trust,
    sourceArtifact: direct ? null : input.sourceArtifact,
    command: direct ? input.command : null,
  }, 'evidence.identity')
  return Object.freeze({
    jobId,
    identity,
    content: new Uint8Array(input.content),
    mediaType: mediaType(input.mediaType, 'evidence.mediaType'),
    producer: parseEvidenceProducer(input.producer),
    candidate: input.candidate === null
      ? null
      : Object.freeze({
        kind: 'complete',
        identity: parseStrongFlowCandidateIdentity(input.candidate, 'evidence.candidate'),
      }),
    createdAtMillis: nonNegativeInteger(input.createdAtMillis, 'evidence.createdAtMillis'),
  })
}

function identityKey(identity: StrongFlowArtifactStoreRecord['identity']): string {
  if (identity.kind === 'artifact') {
    return `artifact:${identity.artifactKind}:${identity.artifactId}`
  }
  if (identity.kind === 'handoff') {
    return identity.target.kind === 'role'
      ? `handoff:role:${identity.target.roleId}:${identity.target.stageRunId}:${identity.target.attemptId}`
      : `handoff:human-review:${identity.handoffId}`
  }
  return `evidence:${identity.evidenceId}`
}

interface PublicationDraft {
  readonly entryKind: StrongFlowArtifactStoreRecord['entryKind']
  readonly identity: StrongFlowArtifactStoreRecord['identity']
  readonly content: Uint8Array
  readonly mediaType: string
  readonly producer: StrongFlowStoredProducer
  readonly candidate: StrongFlowStoredCandidateReference | null
  readonly createdAtMillis: number
}

function publicationMatches(
  recordValue: StrongFlowArtifactStoreRecord,
  draft: PublicationDraft,
  blob: StrongFlowStoredBlobReference,
): boolean {
  return recordValue.entryKind === draft.entryKind
    && isDeepStrictEqual(recordValue.identity, draft.identity)
    && isDeepStrictEqual(recordValue.blob, blob)
    && isDeepStrictEqual(recordValue.producer, draft.producer)
    && isDeepStrictEqual(recordValue.candidate, draft.candidate)
    && recordValue.createdAtMillis === draft.createdAtMillis
}

/** Immutable content-addressed blobs plus append-only per-job metadata. */
export class StrongFlowArtifactStore {
  readonly home: string
  readonly directory: string
  readonly recordsDirectory: string
  readonly blobsDirectory: string
  readonly #manifest: StrongFlowArtifactStoreManifest
  #tail: Promise<void> = Promise.resolve()

  private constructor(
    home: string,
    directory: string,
    manifest: StrongFlowArtifactStoreManifest,
  ) {
    this.home = home
    this.directory = directory
    this.recordsDirectory = join(directory, 'records')
    this.blobsDirectory = blobsRoot(home)
    this.#manifest = manifest
  }

  static async create(
    options: CreateStrongFlowArtifactStoreOptions,
  ): Promise<StrongFlowArtifactStore> {
    const home = validateHome(options.home)
    let jobId: JobIdentifier
    try {
      jobId = JobId(String(options.jobId))
    } catch (error) {
      storeError('INVALID_STORE_OPTIONS', 'artifact store job id is invalid', { cause: error })
    }
    const createdAtMillis = nonNegativeInteger(
      options.createdAtMillis,
      'createdAtMillis',
    )
    const root = storeRoot(home)
    const jobRoot = jobsRoot(home)
    const directory = jobDirectory(home, jobId)
    await mkdir(jobRoot, { recursive: true, mode: 0o700 })
    await mkdir(blobsRoot(home), { recursive: true, mode: 0o700 })
    if (await pathExists(directory)) {
      storeError('JOB_ALREADY_EXISTS', `artifact store for job ${jobId} already exists`)
    }
    const manifest = Object.freeze({
      schemaVersion: STRONGFLOW_ARTIFACT_STORE_SCHEMA_VERSION,
      jobId,
      createdAtMillis,
    })
    const temporary = join(jobRoot, `.creating-${jobKey(jobId)}-${randomUUID()}`)
    try {
      await mkdir(temporary, { mode: 0o700 })
      await mkdir(join(temporary, 'records'), { mode: 0o700 })
      await writeNewFileDurable(
        join(temporary, 'manifest.json'),
        `${JSON.stringify(manifest, null, 2)}\n`,
      )
      await syncDirectory(join(temporary, 'records'))
      await syncDirectory(temporary)
      await rename(temporary, directory)
      await syncDirectory(jobRoot)
      await syncDirectory(root)
      return new StrongFlowArtifactStore(home, directory, manifest)
    } catch (error) {
      await rm(temporary, { recursive: true, force: true })
      if (error instanceof StrongFlowArtifactStoreError) throw error
      if (['EEXIST', 'ENOTEMPTY'].includes(errorCode(error) ?? '')) {
        storeError('JOB_ALREADY_EXISTS', `artifact store for job ${jobId} already exists`, {
          cause: error,
        })
      }
      storeError('STORE_IO_ERROR', `artifact store for job ${jobId} could not be created`, {
        cause: error,
      })
    }
  }

  static async open(homeInput: string, jobIdInput: string): Promise<StrongFlowArtifactStore> {
    const home = validateHome(homeInput)
    let jobId: JobIdentifier
    try {
      jobId = JobId(jobIdInput)
    } catch (error) {
      storeError('INVALID_STORE_OPTIONS', 'artifact store job id is invalid', { cause: error })
    }
    const directory = jobDirectory(home, jobId)
    if (!(await pathExists(directory))) {
      storeError('JOB_NOT_FOUND', `artifact store for job ${jobId} was not found`)
    }
    try {
      if (!(await lstat(directory)).isDirectory()) throw new Error('job path is not a directory')
      const manifest = await loadManifest(join(directory, 'manifest.json'))
      if (manifest.jobId !== jobId || jobKey(manifest.jobId) !== basename(directory)) {
        throw new Error('artifact store manifest does not match its directory')
      }
      const store = new StrongFlowArtifactStore(home, directory, manifest)
      await store.#recordsUnlocked()
      return store
    } catch (error) {
      if (error instanceof StrongFlowArtifactStoreError && error.code === 'JOB_NOT_FOUND') {
        throw error
      }
      storeError('STORE_CORRUPT', `artifact store for job ${jobId} is corrupt`, {
        cause: error,
      })
    }
  }

  get manifest(): StrongFlowArtifactStoreManifest {
    return immutable(structuredClone(this.#manifest))
  }

  async publishArtifact(value: unknown): Promise<StrongFlowArtifactStorePublishReceipt> {
    return this.#serialize(async () => {
      let artifact: StrongFlowArtifact
      try {
        artifact = parseStrongFlowArtifact(value)
      } catch (error) {
        storeError('ENTRY_INVALID', 'artifact failed canonical validation', { cause: error })
      }
      if (artifact.jobId !== this.#manifest.jobId) {
        storeError('JOB_ID_MISMATCH', 'artifact does not belong to this job store')
      }
      const content = Buffer.from(JSON.stringify(artifact), 'utf8')
      return this.#publish({
        entryKind: 'artifact',
        identity: Object.freeze({
          kind: 'artifact',
          artifactKind: artifact.artifactKind,
          artifactId: artifact.artifactId,
        }),
        content,
        mediaType: ARTIFACT_MEDIA_TYPE,
        producer: artifactProducer(artifact),
        candidate: artifactCandidate(artifact),
        createdAtMillis: artifact.createdAtMillis,
      })
    })
  }

  async publishHandoff(value: unknown): Promise<StrongFlowArtifactStorePublishReceipt> {
    return this.#serialize(async () => {
      let handoff: StrongFlowHandoffManifest
      try {
        handoff = parseStrongFlowHandoffManifest(value)
      } catch (error) {
        storeError('ENTRY_INVALID', 'handoff failed canonical validation', { cause: error })
      }
      if (handoff.jobId !== this.#manifest.jobId) {
        storeError('JOB_ID_MISMATCH', 'handoff does not belong to this job store')
      }
      const records = await this.#recordsUnlocked()
      for (const input of handoff.inputs) {
        const source = records.find(recordValue => recordValue.recordId === input.artifactRecordId)
        if (
          source?.entryKind !== 'artifact'
          || source.identity.artifactKind !== input.artifactKind
          || source.identity.artifactId !== input.artifactId
          || source.blob.blobId !== input.blobId
          || source.blob.byteLength !== input.byteLength
        ) storeError('ENTRY_INVALID', 'handoff input does not match a published artifact record')
        await this.#verifyBlob(source.blob)
      }
      const candidate = handoff.candidate === null
        ? null
        : Object.freeze({ kind: 'complete' as const, identity: handoff.candidate })
      return this.#publish({
        entryKind: 'handoff',
        identity: Object.freeze({
          kind: 'handoff',
          handoffId: handoff.handoffId,
          target: handoff.target,
        }),
        content: Buffer.from(JSON.stringify(handoff), 'utf8'),
        mediaType: HANDOFF_MEDIA_TYPE,
        producer: handoff.producer,
        candidate,
        createdAtMillis: handoff.createdAtMillis,
      })
    })
  }

  async publishDirectEvidence(
    value: PublishStrongFlowDirectEvidenceInput,
  ): Promise<StrongFlowArtifactStorePublishReceipt> {
    return this.#serialize(async () => {
      let input: ParsedEvidenceInput
      try {
        input = parseEvidenceInput(value, 'trusted-direct-command')
      } catch (error) {
        if (error instanceof StrongFlowArtifactStoreError) throw error
        storeError('ENTRY_INVALID', 'direct evidence failed canonical validation', {
          cause: error,
        })
      }
      return this.#publishEvidence(input, 'direct-command-evidence')
    })
  }

  async publishModelObservation(
    value: PublishStrongFlowModelObservationInput,
  ): Promise<StrongFlowArtifactStorePublishReceipt> {
    return this.#serialize(async () => {
      let input: ParsedEvidenceInput
      try {
        input = parseEvidenceInput(value, 'model-observation')
      } catch (error) {
        if (error instanceof StrongFlowArtifactStoreError) throw error
        storeError('ENTRY_INVALID', 'model observation failed canonical validation', {
          cause: error,
        })
      }
      return this.#publishEvidence(input, 'model-observation')
    })
  }

  async #publishEvidence(
    input: ParsedEvidenceInput,
    entryKind: 'direct-command-evidence' | 'model-observation',
  ): Promise<StrongFlowArtifactStorePublishReceipt> {
    if (input.jobId !== this.#manifest.jobId) {
      storeError('JOB_ID_MISMATCH', 'evidence does not belong to this job store')
    }
    if (entryKind === 'model-observation') {
      const source = input.identity.sourceArtifact
      const sourceRecord = source === null
        ? undefined
        : (await this.#recordsUnlocked()).find(recordValue => (
          recordValue.entryKind === 'artifact'
          && isDeepStrictEqual(recordValue.identity, source)
        ))
      if (sourceRecord === undefined) {
        storeError('ENTRY_INVALID', 'model observation source artifact is not published')
      }
      if (
        !isDeepStrictEqual(sourceRecord.producer, input.producer)
        || !isDeepStrictEqual(sourceRecord.candidate, input.candidate)
      ) {
        storeError(
          'ENTRY_INVALID',
          'model observation does not match its source artifact producer and candidate',
        )
      }
    }
    return this.#publish({
      entryKind,
      identity: input.identity,
      content: input.content,
      mediaType: input.mediaType,
      producer: input.producer,
      candidate: input.candidate,
      createdAtMillis: input.createdAtMillis,
    })
  }

  async #publish(draft: PublicationDraft): Promise<StrongFlowArtifactStorePublishReceipt> {
    if (draft.content.byteLength > STRONGFLOW_ARTIFACT_STORE_MAX_BLOB_BYTES) {
      storeError('CONTENT_TOO_LARGE', 'content exceeds the artifact store limit')
    }
    const blob = Object.freeze({
      blobId: StrongFlowBlobId(`sha256-${sha256(draft.content)}`),
      byteLength: draft.content.byteLength,
      mediaType: draft.mediaType,
    })
    const wantedIdentity = identityKey(draft.identity)
    let records = await this.#recordsUnlocked()
    let existing = records.find(entry => identityKey(entry.identity) === wantedIdentity)
    if (existing !== undefined) {
      if (!publicationMatches(existing, draft, blob)) {
        storeError('IDENTITY_CONFLICT', 'an immutable artifact, handoff, or evidence id was reused')
      }
      await this.#verifyBlob(blob)
      return Object.freeze({
        outcome: 'already-published',
        blobReused: true,
        record: existing,
      })
    }
    const blobReused = await this.#publishBlob(blob, draft.content)
    for (let attempt = 0; attempt < 100; attempt += 1) {
      records = await this.#recordsUnlocked()
      existing = records.find(entry => identityKey(entry.identity) === wantedIdentity)
      if (existing !== undefined) {
        if (!publicationMatches(existing, draft, blob)) {
          storeError('IDENTITY_CONFLICT', 'an immutable artifact, handoff, or evidence id was reused')
        }
        return Object.freeze({
          outcome: 'already-published',
          blobReused: true,
          record: existing,
        })
      }
      const sequence = nextSequence(records.at(-1)?.sequence)
      const withoutHash = {
        schemaVersion: STRONGFLOW_ARTIFACT_STORE_SCHEMA_VERSION,
        recordId: artifactStoreRecordId(this.#manifest.jobId, sequence),
        jobId: this.#manifest.jobId,
        sequence,
        entryKind: draft.entryKind,
        identity: draft.identity,
        blob,
        producer: draft.producer,
        candidate: draft.candidate,
        createdAtMillis: draft.createdAtMillis,
        previousRecordHash: records.at(-1)?.recordHash ?? null,
      } as Omit<StrongFlowArtifactStoreRecord, 'recordHash'>
      const recordValue = immutable({
        ...withoutHash,
        recordHash: recordHash(withoutHash),
      }) as StrongFlowArtifactStoreRecord
      const published = await this.#publishRecord(recordValue)
      if (!published) continue
      return Object.freeze({ outcome: 'published', blobReused, record: recordValue })
    }
    storeError('STORE_IO_ERROR', 'artifact metadata publication did not converge')
  }

  async read(recordId: StrongFlowArtifactStoreRecordId): Promise<StrongFlowArtifactStoreReadResult> {
    await this.#tail
    const records = await this.#recordsUnlocked()
    const recordValue = records.find(entry => entry.recordId === recordId)
    if (recordValue === undefined) {
      storeError('RECORD_NOT_FOUND', 'artifact store record was not found for this job')
    }
    const content = await this.#readBlob(recordValue.blob)
    if (recordValue.entryKind === 'handoff') {
      let handoff: StrongFlowHandoffManifest
      try {
        const text = new TextDecoder('utf-8', { fatal: true }).decode(content)
        handoff = parseStrongFlowHandoffManifest(JSON.parse(text) as unknown)
      } catch (error) {
        storeError('CONTENT_DIGEST_MISMATCH', 'stored handoff content is invalid', { cause: error })
      }
      const candidate = handoff.candidate === null
        ? null
        : Object.freeze({ kind: 'complete' as const, identity: handoff.candidate })
      if (
        handoff.jobId !== this.#manifest.jobId
        || handoff.handoffId !== recordValue.identity.handoffId
        || !isDeepStrictEqual(handoff.target, recordValue.identity.target)
        || handoff.createdAtMillis !== recordValue.createdAtMillis
        || !isDeepStrictEqual(handoff.producer, recordValue.producer)
        || !isDeepStrictEqual(candidate, recordValue.candidate)
      ) storeError('CONTENT_DIGEST_MISMATCH', 'stored handoff metadata does not match its content')
      return Object.freeze({ record: recordValue, handoff })
    }
    if (recordValue.entryKind !== 'artifact') {
      return Object.freeze({
        record: recordValue,
        content: new Uint8Array(content),
      })
    }
    let artifact: StrongFlowArtifact
    try {
      const text = new TextDecoder('utf-8', { fatal: true }).decode(content)
      artifact = parseStrongFlowArtifact(JSON.parse(text) as unknown)
    } catch (error) {
      storeError('CONTENT_DIGEST_MISMATCH', 'stored artifact content is invalid', { cause: error })
    }
    if (
      artifact.jobId !== this.#manifest.jobId
      || artifact.artifactKind !== recordValue.identity.artifactKind
      || artifact.artifactId !== recordValue.identity.artifactId
      || artifact.createdAtMillis !== recordValue.createdAtMillis
      || !isDeepStrictEqual(artifactProducer(artifact), recordValue.producer)
      || !isDeepStrictEqual(artifactCandidate(artifact), recordValue.candidate)
    ) storeError('CONTENT_DIGEST_MISMATCH', 'stored artifact metadata does not match its content')
    return Object.freeze({ record: recordValue, artifact })
  }

  async findArtifact(
    artifactKind: StrongFlowArtifactKind,
    artifactId: string,
  ): Promise<StrongFlowStoredArtifactContent | undefined> {
    if (!STRONGFLOW_ARTIFACT_KINDS.includes(artifactKind)) {
      storeError('ENTRY_INVALID', 'artifact kind is unsupported')
    }
    portableIdentifier(artifactId, 'artifactId')
    await this.#tail
    const records = await this.#recordsUnlocked()
    const recordValue = records.find(entry => (
      entry.entryKind === 'artifact'
      && entry.identity.artifactKind === artifactKind
      && entry.identity.artifactId === artifactId
    ))
    if (recordValue === undefined) return undefined
    return this.read(recordValue.recordId) as Promise<StrongFlowStoredArtifactContent>
  }

  async findHandoff(handoffId: string): Promise<StrongFlowStoredHandoffContent | undefined> {
    try {
      StrongFlowHandoffId(handoffId)
    } catch (error) {
      storeError('ENTRY_INVALID', 'handoff id is invalid', { cause: error })
    }
    await this.#tail
    const records = await this.#recordsUnlocked()
    const recordValue = records.find(entry => (
      entry.entryKind === 'handoff'
      && entry.identity.handoffId === handoffId
    ))
    if (recordValue === undefined) return undefined
    return this.read(recordValue.recordId) as Promise<StrongFlowStoredHandoffContent>
  }

  async list(queryValue: StrongFlowArtifactStoreListQuery): Promise<StrongFlowArtifactStoreListResult> {
    await this.#tail
    const query = record(queryValue, 'artifact list query')
    exactKeys(query, ['limit'], ['afterSequence', 'attemptId', 'entryKinds'], 'artifact list query')
    const limit = nonNegativeInteger(query.limit, 'artifact list query.limit')
    if (limit === 0 || limit > STRONGFLOW_ARTIFACT_STORE_MAX_LIST_LIMIT) {
      storeError('ENTRY_INVALID', 'artifact list limit is out of range')
    }
    const afterSequence = query.afterSequence === undefined
      ? '0'
      : canonicalSequence(query.afterSequence, 'artifact list query.afterSequence')
    let attemptId: AttemptIdentifier | undefined
    if (query.attemptId !== undefined) {
      try {
        attemptId = AttemptId(String(query.attemptId))
      } catch (error) {
        storeError('ENTRY_INVALID', 'artifact list attempt id is invalid', { cause: error })
      }
    }
    let entryKinds: readonly StrongFlowArtifactStoreRecord['entryKind'][] | undefined
    if (query.entryKinds !== undefined) {
      if (!Array.isArray(query.entryKinds) || query.entryKinds.length === 0) {
        storeError('ENTRY_INVALID', 'artifact list entry kinds must be a non-empty array')
      }
      const allowed = [
        'artifact',
        'handoff',
        'direct-command-evidence',
        'model-observation',
      ] as const
      if (query.entryKinds.some(entry => !allowed.includes(
        entry as StrongFlowArtifactStoreRecord['entryKind'],
      )) || new Set(query.entryKinds).size !== query.entryKinds.length) {
        storeError('ENTRY_INVALID', 'artifact list entry kinds are invalid')
      }
      entryKinds = Object.freeze([
        ...query.entryKinds,
      ]) as readonly StrongFlowArtifactStoreRecord['entryKind'][]
    }
    const records = (await this.#recordsUnlocked()).filter(entry => (
      compareSequence(entry.sequence, afterSequence) > 0
      && (attemptId === undefined
        || (entry.producer.kind === 'role' && entry.producer.attemptId === attemptId))
      && (entryKinds === undefined || entryKinds.includes(entry.entryKind))
    ))
    const selected = records.slice(0, limit)
    return Object.freeze({
      records: Object.freeze(selected),
      nextAfterSequence: records.length > selected.length
        ? selected.at(-1)?.sequence ?? null
        : null,
    })
  }

  async #publishBlob(
    blob: StrongFlowStoredBlobReference,
    content: Uint8Array,
  ): Promise<boolean> {
    const published = blobPath(this.blobsDirectory, blob.blobId)
    const shard = dirname(published)
    await mkdir(shard, { recursive: true, mode: 0o700 })
    if (await pathExists(published)) {
      await this.#verifyBlob(blob)
      return true
    }
    const digest = blob.blobId.slice('sha256-'.length)
    const temporary = join(shard, `.pending-${digest}-${randomUUID()}.blob`)
    try {
      await writeNewFileDurable(temporary, content)
      try {
        await link(temporary, published)
      } catch (error) {
        if (errorCode(error) !== 'EEXIST') throw error
        await this.#verifyBlob(blob)
        return true
      }
      await syncDirectory(shard)
      return false
    } catch (error) {
      if (error instanceof StrongFlowArtifactStoreError) throw error
      storeError('STORE_IO_ERROR', 'content blob could not be published', { cause: error })
    } finally {
      await rm(temporary, { force: true }).catch(() => {})
    }
  }

  async #publishRecord(recordValue: StrongFlowArtifactStoreRecord): Promise<boolean> {
    const temporary = join(
      this.recordsDirectory,
      `.pending-${recordValue.sequence}-${randomUUID()}.json`,
    )
    const published = join(this.recordsDirectory, `${recordValue.sequence}.json`)
    try {
      await writeNewFileDurable(temporary, `${JSON.stringify(recordValue)}\n`)
      try {
        await link(temporary, published)
      } catch (error) {
        if (errorCode(error) === 'EEXIST') return false
        throw error
      }
      await syncDirectory(this.recordsDirectory)
      return true
    } catch (error) {
      storeError('STORE_IO_ERROR', 'artifact metadata record could not be published', {
        cause: error,
      })
    } finally {
      await rm(temporary, { force: true }).catch(() => {})
    }
  }

  async #verifyBlob(blob: StrongFlowStoredBlobReference): Promise<void> {
    await this.#readBlob(blob)
  }

  async #readBlob(blob: StrongFlowStoredBlobReference): Promise<Uint8Array> {
    const path = blobPath(this.blobsDirectory, blob.blobId)
    let info
    try {
      info = await lstat(path)
    } catch (error) {
      if (errorCode(error) === 'ENOENT') {
        storeError('CONTENT_MISSING', 'artifact content blob is missing')
      }
      storeError('STORE_IO_ERROR', 'artifact content blob could not be inspected', { cause: error })
    }
    if (!info.isFile()) {
      storeError('CONTENT_DIGEST_MISMATCH', 'artifact content blob is not a regular file')
    }
    if (
      info.size !== blob.byteLength
      || info.size > STRONGFLOW_ARTIFACT_STORE_MAX_BLOB_BYTES
    ) storeError('CONTENT_DIGEST_MISMATCH', 'artifact content blob size changed')
    let content: Uint8Array
    try {
      content = await readFile(path)
    } catch (error) {
      storeError('STORE_IO_ERROR', 'artifact content blob could not be read', { cause: error })
    }
    if (`sha256-${sha256(content)}` !== blob.blobId) {
      storeError('CONTENT_DIGEST_MISMATCH', 'artifact content blob digest changed')
    }
    return content
  }

  async #recordsUnlocked(): Promise<readonly StrongFlowArtifactStoreRecord[]> {
    try {
      const manifest = await loadManifest(join(this.directory, 'manifest.json'))
      if (
        !isDeepStrictEqual(manifest, this.#manifest)
        || jobKey(manifest.jobId) !== basename(this.directory)
      ) throw new Error('artifact store manifest identity changed')
      const entries = await readdir(this.recordsDirectory, { withFileTypes: true })
      const files: { readonly name: string; readonly sequence: string }[] = []
      for (const entry of entries) {
        if (PENDING_RECORD_PATTERN.test(entry.name)) continue
        const match = RECORD_FILE_PATTERN.exec(entry.name)
        if (!entry.isFile() || match?.[1] === undefined) {
          throw new Error(`unexpected artifact record entry ${entry.name}`)
        }
        files.push({ name: entry.name, sequence: match[1] })
      }
      files.sort((left, right) => compareSequence(left.sequence, right.sequence))
      const records: StrongFlowArtifactStoreRecord[] = []
      let expected = '1'
      let previousHash: StrongFlowArtifactStoreRecordHash | null = null
      const identities = new Set<string>()
      for (const file of files) {
        if (file.sequence !== expected) {
          throw new Error(`artifact record ${file.sequence} appears where ${expected} was expected`)
        }
        const text = await readFile(join(this.recordsDirectory, file.name), 'utf8')
        if (!text.endsWith('\n') || text.slice(0, -1).includes('\n')) {
          throw new Error(`artifact record ${file.name} is incomplete or has extra records`)
        }
        const parsed = parseStoreRecord(JSON.parse(text.slice(0, -1)) as unknown)
        if (
          parsed.jobId !== manifest.jobId
          || parsed.sequence !== file.sequence
          || parsed.previousRecordHash !== previousHash
        ) throw new Error(`artifact record ${file.name} has the wrong ownership or chain`)
        const key = identityKey(parsed.identity)
        if (identities.has(key)) throw new Error(`artifact record identity ${key} is duplicated`)
        identities.add(key)
        records.push(parsed)
        previousHash = parsed.recordHash
        expected = nextSequence(expected)
      }
      return Object.freeze(records)
    } catch (error) {
      if (error instanceof StrongFlowArtifactStoreError
        && ['CONTENT_MISSING', 'CONTENT_DIGEST_MISMATCH'].includes(error.code)) {
        throw error
      }
      storeError('STORE_CORRUPT', `artifact metadata for job ${this.#manifest.jobId} is corrupt`, {
        cause: error,
      })
    }
  }

  #serialize<Result>(operation: () => Promise<Result>): Promise<Result> {
    const current = this.#tail.then(operation, operation)
    this.#tail = current.then(() => {}, () => {})
    return current
  }
}

/** Pending blobs are ignored by readers; this predicate is exported for retention tooling. */
export function isPendingStrongFlowArtifactBlobFile(name: string): boolean {
  return PENDING_BLOB_PATTERN.test(name)
}
