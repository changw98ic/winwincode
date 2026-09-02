import {
  DeliveryId,
  DeliverySpecId,
  SessionBindingId,
  StageRunId,
  type DeliveryId as DeliveryIdentifier,
  type DeliverySpecId as DeliverySpecIdentifier,
  type RepositoryRef,
  type SessionBindingId as SessionBindingIdentifier,
  type StageRunId as StageRunIdentifier,
} from './delivery.js'

export const DELIVERY_CANDIDATE_EVIDENCE_SCHEMA_VERSION = 1 as const

const GIT_OBJECT_PATTERN = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u
const SHA256_PATTERN = /^[0-9a-f]{64}$/u
const CANDIDATE_REF_PATTERN = /^git-candidate:sha256:[0-9a-f]{64}$/u
const MAX_REFERENCE_LENGTH = 4_096
const MAX_PATH_LENGTH = 4_096
const MAX_CHANGED_PATHS = 100_000

export type DeliveryCandidateContractErrorCode =
  | 'INVALID_SHAPE'
  | 'INVALID_VALUE'
  | 'UNSUPPORTED_SCHEMA_VERSION'

export class DeliveryCandidateContractError extends Error {
  readonly code: DeliveryCandidateContractErrorCode
  readonly path: string

  constructor(code: DeliveryCandidateContractErrorCode, path: string, message: string) {
    super(message)
    this.name = 'DeliveryCandidateContractError'
    this.code = code
    this.path = path
  }
}

export interface DeliveryCandidatePathFact {
  readonly path: string
  readonly state: 'present' | 'deleted'
  readonly objectId: string | null
}

export interface FreezeDeliveryCandidateInput {
  readonly producerStageRunId: StageRunIdentifier | string
  readonly producerSessionBindingId: SessionBindingIdentifier | string
  readonly baseCommitId: string
  readonly baseTreeId: string
  readonly candidateCommitId: string
  readonly candidateTreeId: string
  readonly diffSha256: string
  readonly changedPaths: readonly DeliveryCandidatePathFact[]
}

/** Rebuildable identity for one exact Git candidate; it is not persisted as a Delivery object. */
export interface FrozenDeliveryCandidate {
  readonly schemaVersion: typeof DELIVERY_CANDIDATE_EVIDENCE_SCHEMA_VERSION
  readonly candidateRef: string
  readonly deliveryId: DeliveryIdentifier
  readonly deliverySpecId: DeliverySpecIdentifier
  readonly deliverySpecRevision: number
  readonly repositoryKind: RepositoryRef['kind']
  readonly repositoryLocator: string
  readonly baseRevision: string
  readonly producerStageRunId: StageRunIdentifier
  readonly producerSessionBindingId: SessionBindingIdentifier
  readonly baseCommitId: string
  readonly baseTreeId: string
  readonly candidateCommitId: string
  readonly candidateTreeId: string
  readonly diffSha256: string
  readonly changedPaths: readonly DeliveryCandidatePathFact[]
}

function candidateError(
  code: DeliveryCandidateContractErrorCode,
  path: string,
  message: string,
): never {
  throw new DeliveryCandidateContractError(code, path, message)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (!isRecord(value)) candidateError('INVALID_SHAPE', path, `${path} must be an object`)
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
    candidateError('INVALID_SHAPE', path, `${path} has an unexpected shape`)
  }
}

function boundedText(value: unknown, path: string): string {
  if (typeof value !== 'string'
    || value.trim().length === 0
    || value.length > MAX_REFERENCE_LENGTH
    || /[\u0000-\u001f\u007f]/u.test(value)) {
    candidateError('INVALID_VALUE', path, `${path} must be bounded text`)
  }
  return value
}

function positiveInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 1 || Object.is(value, -0)) {
    candidateError('INVALID_VALUE', path, `${path} must be a positive safe integer`)
  }
  return Number(value)
}

function pattern(value: unknown, expected: RegExp, path: string, label: string): string {
  if (typeof value !== 'string' || !expected.test(value)) {
    candidateError('INVALID_VALUE', path, `${path} must be ${label}`)
  }
  return value
}

function portablePath(value: unknown, path: string): string {
  if (typeof value !== 'string'
    || value.length === 0
    || value.length > MAX_PATH_LENGTH
    || value.startsWith('/')
    || value.includes('\\')
    || /^[A-Za-z]:/u.test(value)
    || /[\u0000-\u001f\u007f]/u.test(value)) {
    candidateError('INVALID_VALUE', path, `${path} must be a portable relative path`)
  }
  if (value.split('/').some(segment => segment.length === 0 || segment === '.' || segment === '..')) {
    candidateError('INVALID_VALUE', path, `${path} contains an invalid path segment`)
  }
  return value
}

function identifier<Identifier>(
  value: unknown,
  path: string,
  factory: (input: string) => Identifier,
): Identifier {
  try {
    if (typeof value !== 'string') throw new Error(`${path} must be a string`)
    return factory(value)
  } catch {
    return candidateError('INVALID_VALUE', path, `${path} is invalid`)
  }
}

/** Validate a frozen candidate at an API boundary before checking it against a Delivery. */
export function parseFrozenDeliveryCandidate(
  value: unknown,
  path = 'candidate',
): FrozenDeliveryCandidate {
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'candidateRef',
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'repositoryKind',
    'repositoryLocator',
    'baseRevision',
    'producerStageRunId',
    'producerSessionBindingId',
    'baseCommitId',
    'baseTreeId',
    'candidateCommitId',
    'candidateTreeId',
    'diffSha256',
    'changedPaths',
  ], path)
  if (input.schemaVersion !== DELIVERY_CANDIDATE_EVIDENCE_SCHEMA_VERSION) {
    candidateError(
      'UNSUPPORTED_SCHEMA_VERSION',
      `${path}.schemaVersion`,
      `${path}.schemaVersion is unsupported`,
    )
  }
  if (input.repositoryKind !== 'local-git' && input.repositoryKind !== 'github') {
    candidateError('INVALID_VALUE', `${path}.repositoryKind`, 'repository kind is unsupported')
  }
  if (!Array.isArray(input.changedPaths) || input.changedPaths.length > MAX_CHANGED_PATHS) {
    candidateError('INVALID_VALUE', `${path}.changedPaths`, 'changed paths must be bounded')
  }
  const changedPaths = input.changedPaths.map((entry, index) => {
    const entryPath = `${path}.changedPaths[${String(index)}]`
    const item = record(entry, entryPath)
    exactKeys(item, ['path', 'state', 'objectId'], entryPath)
    if (item.state !== 'present' && item.state !== 'deleted') {
      candidateError('INVALID_VALUE', `${entryPath}.state`, 'candidate path state is unsupported')
    }
    const objectId = item.objectId === null
      ? null
      : pattern(item.objectId, GIT_OBJECT_PATTERN, `${entryPath}.objectId`, 'a Git object id')
    if ((item.state === 'present') !== (objectId !== null)) {
      candidateError('INVALID_VALUE', entryPath, 'candidate path state does not match object id')
    }
    return Object.freeze({
      path: portablePath(item.path, `${entryPath}.path`),
      state: item.state,
      objectId,
    })
  })
  if (new Set(changedPaths.map(entry => entry.path)).size !== changedPaths.length) {
    candidateError('INVALID_VALUE', `${path}.changedPaths`, 'changed paths contain duplicates')
  }
  return Object.freeze({
    schemaVersion: DELIVERY_CANDIDATE_EVIDENCE_SCHEMA_VERSION,
    candidateRef: pattern(
      input.candidateRef,
      CANDIDATE_REF_PATTERN,
      `${path}.candidateRef`,
      'a frozen candidate reference',
    ),
    deliveryId: identifier(input.deliveryId, `${path}.deliveryId`, DeliveryId),
    deliverySpecId: identifier(input.deliverySpecId, `${path}.deliverySpecId`, DeliverySpecId),
    deliverySpecRevision: positiveInteger(
      input.deliverySpecRevision,
      `${path}.deliverySpecRevision`,
    ),
    repositoryKind: input.repositoryKind,
    repositoryLocator: boundedText(input.repositoryLocator, `${path}.repositoryLocator`),
    baseRevision: boundedText(input.baseRevision, `${path}.baseRevision`),
    producerStageRunId: identifier(
      input.producerStageRunId,
      `${path}.producerStageRunId`,
      StageRunId,
    ),
    producerSessionBindingId: identifier(
      input.producerSessionBindingId,
      `${path}.producerSessionBindingId`,
      SessionBindingId,
    ),
    baseCommitId: pattern(
      input.baseCommitId,
      GIT_OBJECT_PATTERN,
      `${path}.baseCommitId`,
      'a Git object id',
    ),
    baseTreeId: pattern(
      input.baseTreeId,
      GIT_OBJECT_PATTERN,
      `${path}.baseTreeId`,
      'a Git object id',
    ),
    candidateCommitId: pattern(
      input.candidateCommitId,
      GIT_OBJECT_PATTERN,
      `${path}.candidateCommitId`,
      'a Git object id',
    ),
    candidateTreeId: pattern(
      input.candidateTreeId,
      GIT_OBJECT_PATTERN,
      `${path}.candidateTreeId`,
      'a Git object id',
    ),
    diffSha256: pattern(
      input.diffSha256,
      SHA256_PATTERN,
      `${path}.diffSha256`,
      'a lowercase SHA-256 digest',
    ),
    changedPaths: Object.freeze(changedPaths),
  })
}
