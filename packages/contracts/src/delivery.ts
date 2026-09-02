/** Canonical WinWinCode delivery facts. Codex and DSH runtime facts stay external. */

export const DELIVERY_SCHEMA_VERSION = 3 as const

export const MAX_DELIVERY_REWORK_ATTEMPTS = 100 as const

export const DELIVERY_STATUSES = Object.freeze([
  'draft',
  'clarifying',
  'ready',
  'planning',
  'plan-review',
  'executing',
  'verifying',
  'reworking',
  'needs-attention',
  'ready-to-deliver',
  'delivered',
] as const)

export type DeliveryStatus = typeof DELIVERY_STATUSES[number]

export const DELIVERY_TASK_STATUSES = Object.freeze([
  'pending',
  'active',
  'blocked',
  'verifying',
  'completed',
  'failed',
] as const)

export type DeliveryTaskStatus = typeof DELIVERY_TASK_STATUSES[number]

export const DELIVERY_STAGES = Object.freeze([
  'clarifying',
  'planning',
  'plan-review',
  'executing',
  'verifying',
  'reworking',
  'delivery-review',
] as const)

export type DeliveryStage = typeof DELIVERY_STAGES[number]

export const STAGE_RUN_STATUSES = Object.freeze([
  'running',
  'waiting',
  'succeeded',
  'failed',
  'cancelled',
] as const)

export type StageRunStatus = typeof STAGE_RUN_STATUSES[number]
export type StageRunActorType = 'codex' | 'human'

export const ATTENTION_ITEM_TYPES = Object.freeze([
  'requirement_question',
  'decision_required',
  'verification_blocked',
  'scope_change',
  'delivery_approval',
] as const)

export type AttentionItemType = typeof ATTENTION_ITEM_TYPES[number]
export type AttentionItemStatus = 'open' | 'resolved' | 'dismissed'

export const EVIDENCE_REF_TYPES = Object.freeze([
  'test',
  'command',
  'diff',
  'file',
  'commit',
  'pull_request',
  'runtime_event',
  'review_finding',
] as const)

export type EvidenceRefType = typeof EVIDENCE_REF_TYPES[number]

export const CRITERION_VERDICTS = Object.freeze([
  'pass',
  'fail',
  'inconclusive',
  'infra_error',
] as const)

export type CriterionVerdict = typeof CRITERION_VERDICTS[number]
export type DeliveryVerdictStatus = CriterionVerdict

export type DeliveryValidationErrorCode =
  | 'INVALID_SHAPE'
  | 'UNSUPPORTED_SCHEMA_VERSION'
  | 'INVALID_IDENTIFIER'
  | 'INVALID_VALUE'
  | 'DUPLICATE_ID'
  | 'RELATIONSHIP_MISMATCH'
  | 'INVALID_VERDICT'

export class DeliveryValidationError extends Error {
  readonly code: DeliveryValidationErrorCode
  readonly path: string

  constructor(
    code: DeliveryValidationErrorCode,
    path: string,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'DeliveryValidationError'
    this.code = code
    this.path = path
  }
}

declare const deliveryIdentifierBrand: unique symbol

type DeliveryIdentifier<Name extends string> = string & {
  readonly [deliveryIdentifierBrand]: Name
}

export type DeliveryId = DeliveryIdentifier<'DeliveryId'>
export type DeliverySpecId = DeliveryIdentifier<'DeliverySpecId'>
export type AcceptanceCriterionId = DeliveryIdentifier<'AcceptanceCriterionId'>
export type DeliveryTaskId = DeliveryIdentifier<'DeliveryTaskId'>
export type StageRunId = DeliveryIdentifier<'StageRunId'>
export type SessionBindingId = DeliveryIdentifier<'SessionBindingId'>
export type AttentionItemId = DeliveryIdentifier<'AttentionItemId'>
export type EvidenceRefId = DeliveryIdentifier<'EvidenceRefId'>
export type CriterionResultId = DeliveryIdentifier<'CriterionResultId'>
export type DeliveryVerdictId = DeliveryIdentifier<'DeliveryVerdictId'>

const PORTABLE_IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,199}$/u
const DELIVERY_ID_PATTERN = /^dlv_[0-9A-HJKMNP-TV-Z]{26}$/u
const CROCKFORD_BASE32_ALPHABET = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'
const ULID_TIMESTAMP_LIMIT = 2 ** 48
const GITHUB_ISSUE_DELIVERY_ID_NAMESPACE = 'winwincode.github-issue-delivery-id.v1'
const SHA256_INITIAL_STATE = Object.freeze([
  0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
  0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
])
const SHA256_ROUND_CONSTANTS = Object.freeze([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5,
  0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
  0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
  0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
  0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
  0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
  0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
  0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
  0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
])
const GITHUB_REPOSITORY_PATTERN = /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})\/[A-Za-z0-9._-]{1,100}$/u
const MAX_TEXT_LENGTH = 65_536
const MAX_REFERENCE_LENGTH = 4_096
const MAX_COLLECTION_LENGTH = 1_000

function deliveryError(
  code: DeliveryValidationErrorCode,
  path: string,
  message: string,
  options?: ErrorOptions,
): never {
  throw new DeliveryValidationError(code, path, message, options)
}

function deliveryIdentifier<Name extends string>(
  value: string,
  name: Name,
): DeliveryIdentifier<Name> {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    deliveryError(
      'INVALID_IDENTIFIER',
      name,
      `${name} must be a portable identifier of at most 200 characters`,
    )
  }
  return value as DeliveryIdentifier<Name>
}

export function DeliveryId(value: string): DeliveryId {
  if (typeof value !== 'string' || !DELIVERY_ID_PATTERN.test(value)) {
    deliveryError(
      'INVALID_IDENTIFIER',
      'DeliveryId',
      'DeliveryId must use dlv_ followed by 26 uppercase Crockford Base32 characters',
    )
  }
  return value as DeliveryId
}

function rotateRight(value: number, bits: number): number {
  return (value >>> bits) | (value << (32 - bits))
}

function sha256Bytes(value: string): Uint8Array {
  const input = Uint8Array.from(value, character => {
    const code = character.charCodeAt(0)
    if (code > 0x7f) throw new TypeError('GitHub issue identity bytes must be ASCII')
    return code
  })
  const paddedLength = Math.ceil((input.length + 9) / 64) * 64
  const padded = new Uint8Array(paddedLength)
  padded.set(input)
  padded[input.length] = 0x80
  const view = new DataView(padded.buffer)
  const bitLength = input.length * 8
  view.setUint32(paddedLength - 8, Math.floor(bitLength / (2 ** 32)), false)
  view.setUint32(paddedLength - 4, bitLength >>> 0, false)
  const state = Uint32Array.from(SHA256_INITIAL_STATE)
  const words = new Uint32Array(64)
  for (let offset = 0; offset < paddedLength; offset += 64) {
    for (let index = 0; index < 16; index += 1) {
      words[index] = view.getUint32(offset + index * 4, false)
    }
    for (let index = 16; index < 64; index += 1) {
      const previous15 = words[index - 15]!
      const previous2 = words[index - 2]!
      const sigma0 = rotateRight(previous15, 7)
        ^ rotateRight(previous15, 18)
        ^ (previous15 >>> 3)
      const sigma1 = rotateRight(previous2, 17)
        ^ rotateRight(previous2, 19)
        ^ (previous2 >>> 10)
      words[index] = (words[index - 16]! + sigma0 + words[index - 7]! + sigma1) >>> 0
    }
    let a = state[0]!
    let b = state[1]!
    let c = state[2]!
    let d = state[3]!
    let e = state[4]!
    let f = state[5]!
    let g = state[6]!
    let h = state[7]!
    for (let index = 0; index < 64; index += 1) {
      const sum1 = rotateRight(e, 6) ^ rotateRight(e, 11) ^ rotateRight(e, 25)
      const choice = (e & f) ^ (~e & g)
      const temporary1 = (h + sum1 + choice + SHA256_ROUND_CONSTANTS[index]! + words[index]!) >>> 0
      const sum0 = rotateRight(a, 2) ^ rotateRight(a, 13) ^ rotateRight(a, 22)
      const majority = (a & b) ^ (a & c) ^ (b & c)
      const temporary2 = (sum0 + majority) >>> 0
      h = g
      g = f
      f = e
      e = (d + temporary1) >>> 0
      d = c
      c = b
      b = a
      a = (temporary1 + temporary2) >>> 0
    }
    state[0] = (state[0]! + a) >>> 0
    state[1] = (state[1]! + b) >>> 0
    state[2] = (state[2]! + c) >>> 0
    state[3] = (state[3]! + d) >>> 0
    state[4] = (state[4]! + e) >>> 0
    state[5] = (state[5]! + f) >>> 0
    state[6] = (state[6]! + g) >>> 0
    state[7] = (state[7]! + h) >>> 0
  }
  const digest = new Uint8Array(32)
  const digestView = new DataView(digest.buffer)
  for (let index = 0; index < state.length; index += 1) {
    digestView.setUint32(index * 4, state[index]!, false)
  }
  return digest
}

function deliveryIdFrom128Bits(bytes: Uint8Array): DeliveryId {
  if (bytes.length < 16) throw new TypeError('Delivery identity requires 128 bits')
  let value = 0n
  for (let index = 0; index < 16; index += 1) {
    value = (value << 8n) | BigInt(bytes[index]!)
  }
  let encoded = ''
  for (let index = 0; index < 26; index += 1) {
    encoded = `${CROCKFORD_BASE32_ALPHABET[Number(value & 31n)]}${encoded}`
    value >>= 5n
  }
  return DeliveryId(`dlv_${encoded}`)
}

/** Generate one canonical, time-sortable Delivery identity for a create command. */
export function generateDeliveryId(createdAtMillis = Date.now()): DeliveryId {
  if (!Number.isSafeInteger(createdAtMillis)
    || createdAtMillis < 0
    || createdAtMillis >= ULID_TIMESTAMP_LIMIT) {
    deliveryError(
      'INVALID_VALUE',
      'createdAtMillis',
      'createdAtMillis must fit the 48-bit ULID timestamp range',
    )
  }
  const randomBytes = new Uint8Array(10)
  globalThis.crypto.getRandomValues(randomBytes)
  let value = BigInt(createdAtMillis)
  for (const byte of randomBytes) value = (value << 8n) | BigInt(byte)
  const bytes = new Uint8Array(16)
  for (let index = 15; index >= 0; index -= 1) {
    bytes[index] = Number(value & 0xffn)
    value >>= 8n
  }
  return deliveryIdFrom128Bits(bytes)
}

export function DeliverySpecId(value: string): DeliverySpecId {
  return deliveryIdentifier(value, 'DeliverySpecId')
}

export function AcceptanceCriterionId(value: string): AcceptanceCriterionId {
  return deliveryIdentifier(value, 'AcceptanceCriterionId')
}

export function DeliveryTaskId(value: string): DeliveryTaskId {
  return deliveryIdentifier(value, 'DeliveryTaskId')
}

export function StageRunId(value: string): StageRunId {
  return deliveryIdentifier(value, 'StageRunId')
}

export function SessionBindingId(value: string): SessionBindingId {
  return deliveryIdentifier(value, 'SessionBindingId')
}

export function AttentionItemId(value: string): AttentionItemId {
  return deliveryIdentifier(value, 'AttentionItemId')
}

export function EvidenceRefId(value: string): EvidenceRefId {
  return deliveryIdentifier(value, 'EvidenceRefId')
}

export function CriterionResultId(value: string): CriterionResultId {
  return deliveryIdentifier(value, 'CriterionResultId')
}

export function DeliveryVerdictId(value: string): DeliveryVerdictId {
  return deliveryIdentifier(value, 'DeliveryVerdictId')
}

export interface RepositoryRef {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly kind: 'local-git' | 'github'
  readonly locator: string
}

/** Minimal external work identity. GitHub remains the owner of all issue fields. */
export interface GitHubIssueSourceRef {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly provider: 'github'
  readonly kind: 'issue'
  readonly repository: string
  readonly number: number
}

/** One intended GitHub pull request, without copying pull-request state. */
export interface GitHubPullRequestTargetRef {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly provider: 'github'
  readonly kind: 'pull-request'
  readonly repository: string
  readonly baseBranch: string
  readonly headRepository: string
  readonly headBranch: string
}

export type DeliverySourceRef = GitHubIssueSourceRef
export type DeliveryPublicationTarget = GitHubPullRequestTargetRef

export interface AcceptanceCriterion {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly id: AcceptanceCriterionId
  readonly description: string
  readonly verificationMethod: string | null
  readonly required: boolean
}

export interface DeliverySpec {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly id: DeliverySpecId
  readonly deliveryId: DeliveryId
  readonly revision: number
  readonly title: string
  readonly goal: string
  readonly scope: readonly string[]
  readonly outOfScope: readonly string[]
  readonly constraints: readonly string[]
  readonly acceptanceCriteria: readonly AcceptanceCriterion[]
  readonly sourceRef: DeliverySourceRef | null
  readonly publicationTarget: DeliveryPublicationTarget | null
  readonly repository: RepositoryRef
  readonly baseRevision: string
  /** Total candidate-writing rework StageRuns allowed for this approved spec. */
  readonly maxReworkAttempts: number
  readonly createdAtMillis: number
}

export interface DeliveryTask {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly id: DeliveryTaskId
  readonly deliveryId: DeliveryId
  readonly title: string
  readonly goal: string
  readonly acceptanceCriterionIds: readonly AcceptanceCriterionId[]
  readonly blockedByTaskIds: readonly DeliveryTaskId[]
  readonly owner: string | null
  readonly status: DeliveryTaskStatus
}

export interface StageRun {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly id: StageRunId
  readonly deliveryId: DeliveryId
  readonly deliveryTaskId: DeliveryTaskId | null
  readonly stage: DeliveryStage
  readonly actorType: StageRunActorType
  readonly role: string
  readonly status: StageRunStatus
  readonly attempt: number
  readonly startedAtMillis: number
  readonly finishedAtMillis: number | null
}

export interface SessionBinding {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly id: SessionBindingId
  readonly deliveryId: DeliveryId
  readonly stageRunId: StageRunId
  readonly dshSessionId: string | null
  readonly codexSessionId: string | null
  readonly boundAtMillis: number
}

export interface AttentionOption {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly id: string
  readonly label: string
  readonly description: string
}

export interface AttentionItem {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly id: AttentionItemId
  readonly deliveryId: DeliveryId
  readonly deliverySpecId: DeliverySpecId
  readonly stageRunId: StageRunId | null
  readonly type: AttentionItemType
  readonly title: string
  readonly context: string
  readonly options: readonly AttentionOption[]
  readonly assignedTo: string | null
  readonly blocking: boolean
  readonly status: AttentionItemStatus
  readonly resolution: string | null
  readonly resolvedBy: string | null
  readonly createdAtMillis: number
  readonly resolvedAtMillis: number | null
}

export interface EvidenceRef {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly id: EvidenceRefId
  readonly deliveryId: DeliveryId
  readonly deliverySpecId: DeliverySpecId
  readonly deliverySpecRevision: number
  readonly stageRunId: StageRunId
  readonly sessionBindingId: SessionBindingId
  readonly candidateRef: string
  readonly type: EvidenceRefType
  readonly sourceRef: string
  readonly createdAtMillis: number
}

export interface CriterionResult {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly id: CriterionResultId
  readonly deliveryId: DeliveryId
  readonly deliverySpecId: DeliverySpecId
  readonly criterionId: AcceptanceCriterionId
  readonly candidateRef: string
  readonly verdict: CriterionVerdict
  readonly evidenceRefs: readonly EvidenceRefId[]
  readonly explanation: string
  readonly evaluatedAtMillis: number
}

export interface DeliveryVerdict {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly id: DeliveryVerdictId
  readonly deliveryId: DeliveryId
  readonly deliverySpecId: DeliverySpecId
  readonly candidateRef: string
  readonly status: DeliveryVerdictStatus
  readonly criteria: readonly CriterionResult[]
  readonly unresolvedFindings: readonly string[]
  readonly producedAtMillis: number
}

export interface Delivery {
  readonly schemaVersion: typeof DELIVERY_SCHEMA_VERSION
  readonly id: DeliveryId
  readonly revision: number
  readonly status: DeliveryStatus
  readonly spec: DeliverySpec
  readonly tasks: readonly DeliveryTask[]
  readonly stageRuns: readonly StageRun[]
  readonly sessionBindings: readonly SessionBinding[]
  readonly attentionItems: readonly AttentionItem[]
  readonly evidence: readonly EvidenceRef[]
  readonly verdict: DeliveryVerdict | null
  readonly createdAtMillis: number
  readonly updatedAtMillis: number
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (!isRecord(value)) deliveryError('INVALID_SHAPE', path, `${path} must be an object`)
  return value
}

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  keys: readonly string[],
  path: string,
): void {
  const expected = new Set(keys)
  if (Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))) {
    deliveryError('INVALID_SHAPE', path, `${path} has an unexpected shape`)
  }
}

function schemaVersion(value: unknown, path: string): typeof DELIVERY_SCHEMA_VERSION {
  if (value !== DELIVERY_SCHEMA_VERSION) {
    deliveryError(
      'UNSUPPORTED_SCHEMA_VERSION',
      path,
      `${path} must be ${String(DELIVERY_SCHEMA_VERSION)}`,
    )
  }
  return DELIVERY_SCHEMA_VERSION
}

function boundedText(
  value: unknown,
  path: string,
  maximum = MAX_TEXT_LENGTH,
): string {
  if (typeof value !== 'string'
    || value.trim().length === 0
    || value.length > maximum
    || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value)) {
    deliveryError('INVALID_VALUE', path, `${path} must be non-empty bounded text`)
  }
  return value
}

function nullableText(
  value: unknown,
  path: string,
  maximum = MAX_TEXT_LENGTH,
): string | null {
  return value === null ? null : boundedText(value, path, maximum)
}

function portableReference(value: unknown, path: string): string {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    deliveryError('INVALID_IDENTIFIER', path, `${path} must be a portable identifier`)
  }
  return value
}

function identifierAt<Identifier>(
  value: unknown,
  path: string,
  factory: (input: string) => Identifier,
): Identifier {
  try {
    if (typeof value !== 'string') throw new Error('identifier must be a string')
    return factory(value)
  } catch (error) {
    deliveryError('INVALID_IDENTIFIER', path, `${path} is invalid`, { cause: error })
  }
}

function enumValue<const Values extends readonly string[]>(
  value: unknown,
  values: Values,
  path: string,
): Values[number] {
  if (typeof value !== 'string' || !values.includes(value)) {
    deliveryError('INVALID_VALUE', path, `${path} is unsupported`)
  }
  return value as Values[number]
}

function booleanValue(value: unknown, path: string): boolean {
  if (typeof value !== 'boolean') deliveryError('INVALID_VALUE', path, `${path} must be boolean`)
  return value
}

function nonNegativeInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Object.is(value, -0)) {
    deliveryError('INVALID_VALUE', path, `${path} must be a non-negative safe integer`)
  }
  return Number(value)
}

function positiveInteger(value: unknown, path: string): number {
  const parsed = nonNegativeInteger(value, path)
  if (parsed === 0) deliveryError('INVALID_VALUE', path, `${path} must be positive`)
  return parsed
}

function reworkAttemptLimit(value: unknown, path: string): number {
  const parsed = nonNegativeInteger(value, path)
  if (parsed > MAX_DELIVERY_REWORK_ATTEMPTS) {
    deliveryError(
      'INVALID_VALUE',
      path,
      `${path} must be at most ${String(MAX_DELIVERY_REWORK_ATTEMPTS)}`,
    )
  }
  return parsed
}

function boundedArray(value: unknown, path: string): readonly unknown[] {
  if (!Array.isArray(value) || value.length > MAX_COLLECTION_LENGTH) {
    deliveryError(
      'INVALID_VALUE',
      path,
      `${path} must be an array with at most ${String(MAX_COLLECTION_LENGTH)} entries`,
    )
  }
  return value
}

function uniqueStrings(
  value: unknown,
  path: string,
  options: { readonly required?: boolean; readonly portable?: boolean } = {},
): readonly string[] {
  const input = boundedArray(value, path)
  if (options.required === true && input.length === 0) {
    deliveryError('INVALID_VALUE', path, `${path} must not be empty`)
  }
  const entries = input.map((entry, index) => (
    options.portable === true
      ? portableReference(entry, `${path}[${String(index)}]`)
      : boundedText(entry, `${path}[${String(index)}]`)
  ))
  if (new Set(entries).size !== entries.length) {
    deliveryError('DUPLICATE_ID', path, `${path} contains duplicate entries`)
  }
  return Object.freeze(entries)
}

function duplicateId(ids: readonly string[], path: string): void {
  if (new Set(ids).size !== ids.length) {
    deliveryError('DUPLICATE_ID', path, `${path} contains duplicate identities`)
  }
}

function githubRepository(value: unknown, path: string): string {
  if (typeof value !== 'string' || !GITHUB_REPOSITORY_PATTERN.test(value)) {
    deliveryError(
      'INVALID_VALUE',
      path,
      `${path} must be a GitHub owner/repository name`,
    )
  }
  return value.toLowerCase()
}

function gitBranch(value: unknown, path: string): string {
  if (typeof value !== 'string'
    || value.length === 0
    || value.length > 255
    || value === '@'
    || value.startsWith('/')
    || value.endsWith('/')
    || value.endsWith('.')
    || value.includes('..')
    || value.includes('@{')
    || value.includes('//')
    || /[\u0000-\u0020\u007f~^:?*\\]/u.test(value)
    || value.includes('[')
    || value.split('/').some(segment => (
      segment.length === 0
      || segment.startsWith('.')
      || segment.endsWith('.lock')
    ))) {
    deliveryError('INVALID_VALUE', path, `${path} must be a valid Git branch name`)
  }
  return value
}

export function parseGitHubIssueSourceRef(
  value: unknown,
  path = 'githubIssueSourceRef',
): GitHubIssueSourceRef {
  const input = record(value, path)
  exactKeys(input, ['schemaVersion', 'provider', 'kind', 'repository', 'number'], path)
  if (input.provider !== 'github' || input.kind !== 'issue') {
    deliveryError('INVALID_VALUE', path, `${path} must identify a GitHub issue`)
  }
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    provider: 'github',
    kind: 'issue',
    repository: githubRepository(input.repository, `${path}.repository`),
    number: positiveInteger(input.number, `${path}.number`),
  })
}

export function parseGitHubPullRequestTargetRef(
  value: unknown,
  path = 'githubPullRequestTargetRef',
): GitHubPullRequestTargetRef {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'provider',
    'kind',
    'repository',
    'baseBranch',
    'headRepository',
    'headBranch',
  ], path)
  if (input.provider !== 'github' || input.kind !== 'pull-request') {
    deliveryError('INVALID_VALUE', path, `${path} must identify a GitHub pull-request target`)
  }
  const repository = githubRepository(input.repository, `${path}.repository`)
  const baseBranch = gitBranch(input.baseBranch, `${path}.baseBranch`)
  const headRepository = githubRepository(input.headRepository, `${path}.headRepository`)
  const headBranch = gitBranch(input.headBranch, `${path}.headBranch`)
  if (repository === headRepository && baseBranch === headBranch) {
    deliveryError(
      'INVALID_VALUE',
      path,
      'GitHub pull-request base and head must identify different branches',
    )
  }
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    provider: 'github',
    kind: 'pull-request',
    repository,
    baseBranch,
    headRepository,
    headBranch,
  })
}

/** Map one normalized GitHub issue to its single stable canonical Delivery identity. */
export function deliveryIdForGitHubIssueSource(
  value: GitHubIssueSourceRef,
): DeliveryId {
  const source = parseGitHubIssueSourceRef(value)
  const identityBytes = [
    GITHUB_ISSUE_DELIVERY_ID_NAMESPACE,
    source.provider,
    source.kind,
    source.repository,
    String(source.number),
  ].join('\0')
  return deliveryIdFrom128Bits(sha256Bytes(identityBytes))
}

export function parseRepositoryRef(value: unknown, path = 'repository'): RepositoryRef {
  const input = record(value, path)
  exactKeys(input, ['schemaVersion', 'kind', 'locator'], path)
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    kind: enumValue(input.kind, ['local-git', 'github'] as const, `${path}.kind`),
    locator: boundedText(input.locator, `${path}.locator`, MAX_REFERENCE_LENGTH),
  })
}

export function parseAcceptanceCriterion(
  value: unknown,
  path = 'acceptanceCriterion',
): AcceptanceCriterion {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'id',
    'description',
    'verificationMethod',
    'required',
  ], path)
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    id: identifierAt(input.id, `${path}.id`, AcceptanceCriterionId),
    description: boundedText(input.description, `${path}.description`),
    verificationMethod: nullableText(input.verificationMethod, `${path}.verificationMethod`),
    required: booleanValue(input.required, `${path}.required`),
  })
}

export function parseDeliverySpec(value: unknown, path = 'deliverySpec'): DeliverySpec {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'id',
    'deliveryId',
    'revision',
    'title',
    'goal',
    'scope',
    'outOfScope',
    'constraints',
    'acceptanceCriteria',
    'sourceRef',
    'publicationTarget',
    'repository',
    'baseRevision',
    'maxReworkAttempts',
    'createdAtMillis',
  ], path)
  const acceptanceCriteria = boundedArray(
    input.acceptanceCriteria,
    `${path}.acceptanceCriteria`,
  ).map((criterion, index) => parseAcceptanceCriterion(
    criterion,
    `${path}.acceptanceCriteria[${String(index)}]`,
  ))
  if (acceptanceCriteria.length === 0 || !acceptanceCriteria.some(criterion => criterion.required)) {
    deliveryError(
      'INVALID_VALUE',
      `${path}.acceptanceCriteria`,
      'delivery spec must contain at least one required acceptance criterion',
    )
  }
  duplicateId(
    acceptanceCriteria.map(criterion => criterion.id),
    `${path}.acceptanceCriteria`,
  )
  const deliveryId = identifierAt(input.deliveryId, `${path}.deliveryId`, DeliveryId)
  const sourceRef = input.sourceRef === null
    ? null
    : parseGitHubIssueSourceRef(input.sourceRef, `${path}.sourceRef`)
  const publicationTarget = input.publicationTarget === null
    ? null
    : parseGitHubPullRequestTargetRef(
      input.publicationTarget,
      `${path}.publicationTarget`,
    )
  if (sourceRef !== null && deliveryId !== deliveryIdForGitHubIssueSource(sourceRef)) {
    deliveryError(
      'RELATIONSHIP_MISMATCH',
      `${path}.deliveryId`,
      'a GitHub issue source must use its stable canonical Delivery identity',
    )
  }
  if (publicationTarget !== null && sourceRef === null) {
    deliveryError(
      'RELATIONSHIP_MISMATCH',
      `${path}.publicationTarget`,
      'a GitHub pull-request target requires a GitHub issue source',
    )
  }
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    id: identifierAt(input.id, `${path}.id`, DeliverySpecId),
    deliveryId,
    revision: positiveInteger(input.revision, `${path}.revision`),
    title: boundedText(input.title, `${path}.title`, 256),
    goal: boundedText(input.goal, `${path}.goal`),
    scope: uniqueStrings(input.scope, `${path}.scope`, { required: true }),
    outOfScope: uniqueStrings(input.outOfScope, `${path}.outOfScope`),
    constraints: uniqueStrings(input.constraints, `${path}.constraints`),
    acceptanceCriteria: Object.freeze(acceptanceCriteria),
    sourceRef,
    publicationTarget,
    repository: parseRepositoryRef(input.repository, `${path}.repository`),
    baseRevision: boundedText(input.baseRevision, `${path}.baseRevision`, MAX_REFERENCE_LENGTH),
    maxReworkAttempts: reworkAttemptLimit(
      input.maxReworkAttempts,
      `${path}.maxReworkAttempts`,
    ),
    createdAtMillis: nonNegativeInteger(input.createdAtMillis, `${path}.createdAtMillis`),
  })
}

export function parseDeliveryTask(value: unknown, path = 'deliveryTask'): DeliveryTask {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'id',
    'deliveryId',
    'title',
    'goal',
    'acceptanceCriterionIds',
    'blockedByTaskIds',
    'owner',
    'status',
  ], path)
  const acceptanceCriterionIds = uniqueStrings(
    input.acceptanceCriterionIds,
    `${path}.acceptanceCriterionIds`,
    { required: true, portable: true },
  ).map(AcceptanceCriterionId)
  const blockedByTaskIds = uniqueStrings(
    input.blockedByTaskIds,
    `${path}.blockedByTaskIds`,
    { portable: true },
  ).map(DeliveryTaskId)
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    id: identifierAt(input.id, `${path}.id`, DeliveryTaskId),
    deliveryId: identifierAt(input.deliveryId, `${path}.deliveryId`, DeliveryId),
    title: boundedText(input.title, `${path}.title`, 256),
    goal: boundedText(input.goal, `${path}.goal`),
    acceptanceCriterionIds: Object.freeze(acceptanceCriterionIds),
    blockedByTaskIds: Object.freeze(blockedByTaskIds),
    owner: nullableText(input.owner, `${path}.owner`, 500),
    status: enumValue(input.status, DELIVERY_TASK_STATUSES, `${path}.status`),
  })
}

export function parseStageRun(value: unknown, path = 'stageRun'): StageRun {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'id',
    'deliveryId',
    'deliveryTaskId',
    'stage',
    'actorType',
    'role',
    'status',
    'attempt',
    'startedAtMillis',
    'finishedAtMillis',
  ], path)
  const status = enumValue(input.status, STAGE_RUN_STATUSES, `${path}.status`)
  const startedAtMillis = nonNegativeInteger(input.startedAtMillis, `${path}.startedAtMillis`)
  const finishedAtMillis = input.finishedAtMillis === null
    ? null
    : nonNegativeInteger(input.finishedAtMillis, `${path}.finishedAtMillis`)
  const active = status === 'running' || status === 'waiting'
  if ((active && finishedAtMillis !== null)
    || (!active && finishedAtMillis === null)
    || (finishedAtMillis !== null && finishedAtMillis < startedAtMillis)) {
    deliveryError(
      'INVALID_VALUE',
      `${path}.finishedAtMillis`,
      'stage run finish time does not match its status or start time',
    )
  }
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    id: identifierAt(input.id, `${path}.id`, StageRunId),
    deliveryId: identifierAt(input.deliveryId, `${path}.deliveryId`, DeliveryId),
    deliveryTaskId: input.deliveryTaskId === null
      ? null
      : identifierAt(input.deliveryTaskId, `${path}.deliveryTaskId`, DeliveryTaskId),
    stage: enumValue(input.stage, DELIVERY_STAGES, `${path}.stage`),
    actorType: enumValue(input.actorType, ['codex', 'human'] as const, `${path}.actorType`),
    role: portableReference(input.role, `${path}.role`),
    status,
    attempt: positiveInteger(input.attempt, `${path}.attempt`),
    startedAtMillis,
    finishedAtMillis,
  })
}

export function parseSessionBinding(
  value: unknown,
  path = 'sessionBinding',
): SessionBinding {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'id',
    'deliveryId',
    'stageRunId',
    'dshSessionId',
    'codexSessionId',
    'boundAtMillis',
  ], path)
  const dshSessionId = input.dshSessionId === null
    ? null
    : portableReference(input.dshSessionId, `${path}.dshSessionId`)
  const codexSessionId = input.codexSessionId === null
    ? null
    : portableReference(input.codexSessionId, `${path}.codexSessionId`)
  if (dshSessionId === null && codexSessionId === null) {
    deliveryError(
      'INVALID_VALUE',
      path,
      'session binding must reference a DSH session, a Codex session, or both',
    )
  }
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    id: identifierAt(input.id, `${path}.id`, SessionBindingId),
    deliveryId: identifierAt(input.deliveryId, `${path}.deliveryId`, DeliveryId),
    stageRunId: identifierAt(input.stageRunId, `${path}.stageRunId`, StageRunId),
    dshSessionId,
    codexSessionId,
    boundAtMillis: nonNegativeInteger(input.boundAtMillis, `${path}.boundAtMillis`),
  })
}

function parseAttentionOption(value: unknown, path: string): AttentionOption {
  const input = record(value, path)
  exactKeys(input, ['schemaVersion', 'id', 'label', 'description'], path)
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    id: portableReference(input.id, `${path}.id`),
    label: boundedText(input.label, `${path}.label`, 256),
    description: boundedText(input.description, `${path}.description`),
  })
}

export function parseAttentionItem(value: unknown, path = 'attentionItem'): AttentionItem {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'id',
    'deliveryId',
    'deliverySpecId',
    'stageRunId',
    'type',
    'title',
    'context',
    'options',
    'assignedTo',
    'blocking',
    'status',
    'resolution',
    'resolvedBy',
    'createdAtMillis',
    'resolvedAtMillis',
  ], path)
  const options = boundedArray(input.options, `${path}.options`).map((option, index) => (
    parseAttentionOption(option, `${path}.options[${String(index)}]`)
  ))
  duplicateId(options.map(option => option.id), `${path}.options`)
  const status = enumValue(
    input.status,
    ['open', 'resolved', 'dismissed'] as const,
    `${path}.status`,
  )
  const createdAtMillis = nonNegativeInteger(input.createdAtMillis, `${path}.createdAtMillis`)
  const resolvedAtMillis = input.resolvedAtMillis === null
    ? null
    : nonNegativeInteger(input.resolvedAtMillis, `${path}.resolvedAtMillis`)
  const resolution = nullableText(input.resolution, `${path}.resolution`)
  const resolvedBy = nullableText(input.resolvedBy, `${path}.resolvedBy`, 500)
  if ((status === 'open' && (resolution !== null || resolvedBy !== null || resolvedAtMillis !== null))
    || (status !== 'open'
      && (resolution === null
        || resolvedBy === null
        || resolvedAtMillis === null
        || resolvedAtMillis < createdAtMillis))) {
    deliveryError(
      'INVALID_VALUE',
      `${path}.status`,
      'attention resolution fields do not match its status',
    )
  }
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    id: identifierAt(input.id, `${path}.id`, AttentionItemId),
    deliveryId: identifierAt(input.deliveryId, `${path}.deliveryId`, DeliveryId),
    deliverySpecId: identifierAt(
      input.deliverySpecId,
      `${path}.deliverySpecId`,
      DeliverySpecId,
    ),
    stageRunId: input.stageRunId === null
      ? null
      : identifierAt(input.stageRunId, `${path}.stageRunId`, StageRunId),
    type: enumValue(input.type, ATTENTION_ITEM_TYPES, `${path}.type`),
    title: boundedText(input.title, `${path}.title`, 256),
    context: boundedText(input.context, `${path}.context`),
    options: Object.freeze(options),
    assignedTo: nullableText(input.assignedTo, `${path}.assignedTo`, 500),
    blocking: booleanValue(input.blocking, `${path}.blocking`),
    status,
    resolution,
    resolvedBy,
    createdAtMillis,
    resolvedAtMillis,
  })
}

export function parseEvidenceRef(value: unknown, path = 'evidenceRef'): EvidenceRef {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'id',
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'stageRunId',
    'sessionBindingId',
    'candidateRef',
    'type',
    'sourceRef',
    'createdAtMillis',
  ], path)
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    id: identifierAt(input.id, `${path}.id`, EvidenceRefId),
    deliveryId: identifierAt(input.deliveryId, `${path}.deliveryId`, DeliveryId),
    deliverySpecId: identifierAt(input.deliverySpecId, `${path}.deliverySpecId`, DeliverySpecId),
    deliverySpecRevision: positiveInteger(
      input.deliverySpecRevision,
      `${path}.deliverySpecRevision`,
    ),
    stageRunId: identifierAt(input.stageRunId, `${path}.stageRunId`, StageRunId),
    sessionBindingId: identifierAt(
      input.sessionBindingId,
      `${path}.sessionBindingId`,
      SessionBindingId,
    ),
    candidateRef: boundedText(input.candidateRef, `${path}.candidateRef`, MAX_REFERENCE_LENGTH),
    type: enumValue(input.type, EVIDENCE_REF_TYPES, `${path}.type`),
    sourceRef: boundedText(input.sourceRef, `${path}.sourceRef`, MAX_REFERENCE_LENGTH),
    createdAtMillis: nonNegativeInteger(input.createdAtMillis, `${path}.createdAtMillis`),
  })
}

export function parseCriterionResult(
  value: unknown,
  path = 'criterionResult',
): CriterionResult {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'id',
    'deliveryId',
    'deliverySpecId',
    'criterionId',
    'candidateRef',
    'verdict',
    'evidenceRefs',
    'explanation',
    'evaluatedAtMillis',
  ], path)
  const verdict = enumValue(input.verdict, CRITERION_VERDICTS, `${path}.verdict`)
  const evidenceRefs = uniqueStrings(
    input.evidenceRefs,
    `${path}.evidenceRefs`,
    { portable: true },
  ).map(EvidenceRefId)
  if ((verdict === 'pass' || verdict === 'fail') && evidenceRefs.length === 0) {
    deliveryError(
      'INVALID_VERDICT',
      `${path}.evidenceRefs`,
      `${verdict} criterion result must cite evidence`,
    )
  }
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    id: identifierAt(input.id, `${path}.id`, CriterionResultId),
    deliveryId: identifierAt(input.deliveryId, `${path}.deliveryId`, DeliveryId),
    deliverySpecId: identifierAt(input.deliverySpecId, `${path}.deliverySpecId`, DeliverySpecId),
    criterionId: identifierAt(input.criterionId, `${path}.criterionId`, AcceptanceCriterionId),
    candidateRef: boundedText(input.candidateRef, `${path}.candidateRef`, MAX_REFERENCE_LENGTH),
    verdict,
    evidenceRefs: Object.freeze(evidenceRefs),
    explanation: boundedText(input.explanation, `${path}.explanation`),
    evaluatedAtMillis: nonNegativeInteger(input.evaluatedAtMillis, `${path}.evaluatedAtMillis`),
  })
}

export function parseDeliveryVerdict(
  value: unknown,
  path = 'deliveryVerdict',
): DeliveryVerdict {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'id',
    'deliveryId',
    'deliverySpecId',
    'candidateRef',
    'status',
    'criteria',
    'unresolvedFindings',
    'producedAtMillis',
  ], path)
  const criteria = boundedArray(input.criteria, `${path}.criteria`).map((criterion, index) => (
    parseCriterionResult(criterion, `${path}.criteria[${String(index)}]`)
  ))
  if (criteria.length === 0) {
    deliveryError('INVALID_VERDICT', `${path}.criteria`, 'delivery verdict must evaluate criteria')
  }
  duplicateId(criteria.map(criterion => criterion.id), `${path}.criteria`)
  duplicateId(criteria.map(criterion => criterion.criterionId), `${path}.criteria`)
  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    id: identifierAt(input.id, `${path}.id`, DeliveryVerdictId),
    deliveryId: identifierAt(input.deliveryId, `${path}.deliveryId`, DeliveryId),
    deliverySpecId: identifierAt(input.deliverySpecId, `${path}.deliverySpecId`, DeliverySpecId),
    candidateRef: boundedText(input.candidateRef, `${path}.candidateRef`, MAX_REFERENCE_LENGTH),
    status: enumValue(input.status, CRITERION_VERDICTS, `${path}.status`),
    criteria: Object.freeze(criteria),
    unresolvedFindings: uniqueStrings(input.unresolvedFindings, `${path}.unresolvedFindings`),
    producedAtMillis: nonNegativeInteger(input.producedAtMillis, `${path}.producedAtMillis`),
  })
}

function assertTaskGraph(tasks: readonly DeliveryTask[], path: string): void {
  const tasksById = new Map(tasks.map(task => [task.id, task]))
  for (const [index, task] of tasks.entries()) {
    for (const dependencyId of task.blockedByTaskIds) {
      if (dependencyId === task.id || !tasksById.has(dependencyId)) {
        deliveryError(
          'RELATIONSHIP_MISMATCH',
          `${path}[${String(index)}].blockedByTaskIds`,
          'delivery task dependency is missing or self-referential',
        )
      }
    }
  }
  const visiting = new Set<DeliveryTaskId>()
  const visited = new Set<DeliveryTaskId>()
  const visit = (task: DeliveryTask): void => {
    if (visiting.has(task.id)) {
      deliveryError('RELATIONSHIP_MISMATCH', path, 'delivery task dependencies contain a cycle')
    }
    if (visited.has(task.id)) return
    visiting.add(task.id)
    for (const dependencyId of task.blockedByTaskIds) visit(tasksById.get(dependencyId)!)
    visiting.delete(task.id)
    visited.add(task.id)
  }
  for (const task of tasks) visit(task)
}

export function parseDelivery(value: unknown, path = 'delivery'): Delivery {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'id',
    'revision',
    'status',
    'spec',
    'tasks',
    'stageRuns',
    'sessionBindings',
    'attentionItems',
    'evidence',
    'verdict',
    'createdAtMillis',
    'updatedAtMillis',
  ], path)
  const id = identifierAt(input.id, `${path}.id`, DeliveryId)
  const spec = parseDeliverySpec(input.spec, `${path}.spec`)
  const tasks = boundedArray(input.tasks, `${path}.tasks`).map((task, index) => (
    parseDeliveryTask(task, `${path}.tasks[${String(index)}]`)
  ))
  const stageRuns = boundedArray(input.stageRuns, `${path}.stageRuns`).map((run, index) => (
    parseStageRun(run, `${path}.stageRuns[${String(index)}]`)
  ))
  const sessionBindings = boundedArray(
    input.sessionBindings,
    `${path}.sessionBindings`,
  ).map((binding, index) => parseSessionBinding(
    binding,
    `${path}.sessionBindings[${String(index)}]`,
  ))
  const attentionItems = boundedArray(
    input.attentionItems,
    `${path}.attentionItems`,
  ).map((item, index) => parseAttentionItem(
    item,
    `${path}.attentionItems[${String(index)}]`,
  ))
  const evidence = boundedArray(input.evidence, `${path}.evidence`).map((reference, index) => (
    parseEvidenceRef(reference, `${path}.evidence[${String(index)}]`)
  ))
  const verdict = input.verdict === null
    ? null
    : parseDeliveryVerdict(input.verdict, `${path}.verdict`)
  const createdAtMillis = nonNegativeInteger(input.createdAtMillis, `${path}.createdAtMillis`)
  const updatedAtMillis = nonNegativeInteger(input.updatedAtMillis, `${path}.updatedAtMillis`)
  if (updatedAtMillis < createdAtMillis) {
    deliveryError('INVALID_VALUE', `${path}.updatedAtMillis`, 'delivery update precedes creation')
  }
  if (spec.deliveryId !== id) {
    deliveryError('RELATIONSHIP_MISMATCH', `${path}.spec.deliveryId`, 'spec belongs to another delivery')
  }
  duplicateId(tasks.map(task => task.id), `${path}.tasks`)
  duplicateId(stageRuns.map(run => run.id), `${path}.stageRuns`)
  duplicateId(sessionBindings.map(binding => binding.id), `${path}.sessionBindings`)
  duplicateId(attentionItems.map(item => item.id), `${path}.attentionItems`)
  duplicateId(evidence.map(reference => reference.id), `${path}.evidence`)

  const criterionIds = new Set(spec.acceptanceCriteria.map(criterion => criterion.id))
  const taskIds = new Set(tasks.map(task => task.id))
  const runsById = new Map(stageRuns.map(run => [run.id, run]))
  const bindingsById = new Map(sessionBindings.map(binding => [binding.id, binding]))
  const evidenceById = new Map(evidence.map(reference => [reference.id, reference]))
  for (const [index, task] of tasks.entries()) {
    if (task.deliveryId !== id
      || task.acceptanceCriterionIds.some(criterionId => !criterionIds.has(criterionId))) {
      deliveryError(
        'RELATIONSHIP_MISMATCH',
        `${path}.tasks[${String(index)}]`,
        'delivery task does not match the delivery or its acceptance criteria',
      )
    }
  }
  assertTaskGraph(tasks, `${path}.tasks`)
  for (const [index, run] of stageRuns.entries()) {
    if (run.deliveryId !== id
      || (run.deliveryTaskId !== null && !taskIds.has(run.deliveryTaskId))) {
      deliveryError(
        'RELATIONSHIP_MISMATCH',
        `${path}.stageRuns[${String(index)}]`,
        'stage run does not match the delivery or a delivery task',
      )
    }
    if (run.stage === 'reworking'
      && (run.actorType !== 'codex'
        || run.role !== 'remediator')) {
      deliveryError(
        'RELATIONSHIP_MISMATCH',
        `${path}.stageRuns[${String(index)}]`,
        'rework stage run must use a Codex remediator',
      )
    }
  }
  if (stageRuns.filter(run => run.stage === 'reworking').length > spec.maxReworkAttempts) {
    deliveryError(
      'RELATIONSHIP_MISMATCH',
      `${path}.stageRuns`,
      'delivery exceeds the approved rework attempt limit',
    )
  }
  for (const [index, binding] of sessionBindings.entries()) {
    const run = runsById.get(binding.stageRunId)
    if (binding.deliveryId !== id
      || run === undefined
      || (run.actorType === 'codex' && binding.codexSessionId === null)) {
      deliveryError(
        'RELATIONSHIP_MISMATCH',
        `${path}.sessionBindings[${String(index)}]`,
        'session binding does not match its delivery and stage actor',
      )
    }
  }
  for (const [index, item] of attentionItems.entries()) {
    if (item.deliveryId !== id
      || item.deliverySpecId !== spec.id
      || (item.stageRunId !== null && !runsById.has(item.stageRunId))) {
      deliveryError(
        'RELATIONSHIP_MISMATCH',
        `${path}.attentionItems[${String(index)}]`,
        'attention item does not match its delivery, current spec, or stage run',
      )
    }
  }
  for (const [index, reference] of evidence.entries()) {
    const run = runsById.get(reference.stageRunId)
    const binding = bindingsById.get(reference.sessionBindingId)
    if (reference.deliveryId !== id
      || reference.deliverySpecId !== spec.id
      || reference.deliverySpecRevision !== spec.revision
      || run === undefined
      || binding === undefined
      || binding.deliveryId !== id
      || binding.stageRunId !== reference.stageRunId
      || reference.createdAtMillis < run.startedAtMillis
      || reference.createdAtMillis < binding.boundAtMillis) {
      deliveryError(
        'RELATIONSHIP_MISMATCH',
        `${path}.evidence[${String(index)}]`,
        'evidence does not match its delivery, current spec revision, stage run, session binding, or binding time',
      )
    }
  }

  const status = enumValue(input.status, DELIVERY_STATUSES, `${path}.status`)
  if (status === 'needs-attention'
    && !attentionItems.some(item => item.blocking && item.status === 'open')) {
    deliveryError(
      'RELATIONSHIP_MISMATCH',
      `${path}.attentionItems`,
      'needs-attention delivery has no open blocking attention item',
    )
  }
  if (verdict !== null) {
    if (verdict.deliveryId !== id || verdict.deliverySpecId !== spec.id) {
      deliveryError(
        'RELATIONSHIP_MISMATCH',
        `${path}.verdict`,
        'delivery verdict does not match the current delivery and spec',
      )
    }
    const resultsByCriterion = new Map(verdict.criteria.map(result => [result.criterionId, result]))
    if (resultsByCriterion.size !== criterionIds.size
      || [...criterionIds].some(criterionId => !resultsByCriterion.has(criterionId))) {
      deliveryError(
        'INVALID_VERDICT',
        `${path}.verdict.criteria`,
        'delivery verdict must evaluate every current acceptance criterion exactly once',
      )
    }
    for (const [index, result] of verdict.criteria.entries()) {
      if (result.deliveryId !== id
        || result.deliverySpecId !== spec.id
        || result.candidateRef !== verdict.candidateRef
        || result.evaluatedAtMillis > verdict.producedAtMillis) {
        deliveryError(
          'RELATIONSHIP_MISMATCH',
          `${path}.verdict.criteria[${String(index)}]`,
          'criterion result does not match the verdict identity or production time',
        )
      }
      for (const evidenceId of result.evidenceRefs) {
        const reference = evidenceById.get(evidenceId)
        if (reference === undefined
          || reference.candidateRef !== verdict.candidateRef
          || reference.createdAtMillis > result.evaluatedAtMillis) {
          deliveryError(
            'RELATIONSHIP_MISMATCH',
            `${path}.verdict.criteria[${String(index)}].evidenceRefs`,
            'criterion result cites missing, later, or foreign-candidate evidence',
          )
        }
      }
    }
    const requiredResults = spec.acceptanceCriteria
      .filter(criterion => criterion.required)
      .map(criterion => resultsByCriterion.get(criterion.id)!)
    const expectedVerdict: DeliveryVerdictStatus = requiredResults.some(
      result => result.verdict === 'fail',
    )
      ? 'fail'
      : requiredResults.some(result => result.verdict === 'infra_error')
        ? 'infra_error'
        : requiredResults.some(result => result.verdict === 'inconclusive')
          || verdict.unresolvedFindings.length > 0
          ? 'inconclusive'
          : 'pass'
    if (verdict.status !== expectedVerdict) {
      deliveryError(
        'INVALID_VERDICT',
        `${path}.verdict.status`,
        `delivery verdict must be ${expectedVerdict} for its required criterion results`,
      )
    }
    if (verdict.status === 'pass'
      && attentionItems.some(item => (
        item.blocking && item.status === 'open' && item.type !== 'delivery_approval'
      ))) {
      deliveryError(
        'INVALID_VERDICT',
        `${path}.verdict.status`,
        'a passing verdict cannot retain open blocking Attention',
      )
    }
  }
  if ((status === 'ready-to-deliver' || status === 'delivered')
    && verdict?.status !== 'pass') {
    deliveryError(
      'INVALID_VERDICT',
      `${path}.status`,
      `${status} requires a passing delivery verdict`,
    )
  }
  if ((status === 'ready-to-deliver' || status === 'delivered')
    && tasks.some(task => task.status !== 'completed')) {
    deliveryError(
      'RELATIONSHIP_MISMATCH',
      `${path}.tasks`,
      `${status} requires every DeliveryTask to be completed`,
    )
  }
  if (status === 'delivered'
    && attentionItems.some(item => item.blocking && item.status === 'open')) {
    deliveryError(
      'RELATIONSHIP_MISMATCH',
      `${path}.attentionItems`,
      'delivered delivery cannot retain blocking attention',
    )
  }

  return Object.freeze({
    schemaVersion: schemaVersion(input.schemaVersion, `${path}.schemaVersion`),
    id,
    revision: positiveInteger(input.revision, `${path}.revision`),
    status,
    spec,
    tasks: Object.freeze(tasks),
    stageRuns: Object.freeze(stageRuns),
    sessionBindings: Object.freeze(sessionBindings),
    attentionItems: Object.freeze(attentionItems),
    evidence: Object.freeze(evidence),
    verdict,
    createdAtMillis,
    updatedAtMillis,
  })
}
