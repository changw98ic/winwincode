import { createHash } from 'node:crypto'

import {
  DELIVERY_SCHEMA_VERSION,
  DELIVERY_CANDIDATE_EVIDENCE_SCHEMA_VERSION,
  STRONGFLOW_ROLE_IDS,
  EvidenceRefId,
  SessionBindingId,
  StageRunId,
  parseDelivery,
  parseEvidenceRef,
  parseFrozenDeliveryCandidate,
  strongFlowRoleWorkspaceMode,
  type DeliveryCandidatePathFact,
  type Delivery,
  type EvidenceRef,
  type EvidenceRefId as EvidenceIdentifier,
  type EvidenceRefType,
  type FreezeDeliveryCandidateInput,
  type FrozenDeliveryCandidate,
  type RuntimeEvent,
  type SessionBinding,
  type SessionBindingId as SessionBindingIdentifier,
  type StageRun,
  type StageRunId as StageRunIdentifier,
  type StrongFlowRoleId,
} from '@winwincode/contracts'

export {
  DELIVERY_CANDIDATE_EVIDENCE_SCHEMA_VERSION,
  type DeliveryCandidatePathFact,
  type FreezeDeliveryCandidateInput,
  type FrozenDeliveryCandidate,
} from '@winwincode/contracts'

import {
  AcceptanceVerificationError,
  assertAcceptanceVerificationInputCurrent,
  type AcceptanceVerificationInput,
} from './acceptance-verification.js'
import {
  DeliveryRuntimeProjection,
  DeliveryRuntimeProjectionError,
  deliveryRuntimeEvidenceOutcome,
  type DeliveryRuntimeEvidenceLink,
  type DeliveryRuntimeEvidenceOutcome,
} from './delivery-runtime-projection.js'

const GIT_OBJECT_PATTERN = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u
const SHA256_PATTERN = /^[0-9a-f]{64}$/u
const CANDIDATE_REF_PATTERN = /^git-candidate:sha256:[0-9a-f]{64}$/u
const MAX_PATH_LENGTH = 4_096

export type DeliveryCandidateEvidenceErrorCode =
  | 'INVALID_DELIVERY'
  | 'INVALID_CANDIDATE'
  | 'INVALID_EVIDENCE'
  | 'CANDIDATE_STALE'
  | 'ACCEPTANCE_STALE'
  | 'EVIDENCE_STAGE_MISMATCH'
  | 'EVIDENCE_SESSION_MISMATCH'
  | 'EVIDENCE_SOURCE_MISSING'
  | 'EVIDENCE_SOURCE_AMBIGUOUS'
  | 'EVIDENCE_TYPE_MISMATCH'
  | 'EVIDENCE_CANDIDATE_MISMATCH'
  | 'VERIFIER_POLICY_MISMATCH'
  | 'VERIFIER_WRITE_OBSERVED'

export class DeliveryCandidateEvidenceError extends Error {
  readonly code: DeliveryCandidateEvidenceErrorCode

  constructor(
    code: DeliveryCandidateEvidenceErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'DeliveryCandidateEvidenceError'
    this.code = code
  }
}

export type DeliveryRuntimeEvidenceType = Exclude<EvidenceRefType, 'pull_request'>

export type DeliveryEvidenceSource =
  | {
      readonly kind: 'runtime-event'
      readonly type: DeliveryRuntimeEvidenceType
      readonly eventId: string
    }
  | { readonly kind: 'candidate-commit' }
  | { readonly kind: 'candidate-diff' }
  | { readonly kind: 'candidate-file'; readonly path: string }

export interface ResolveDeliveryEvidenceInput {
  readonly delivery: Delivery
  readonly acceptance: AcceptanceVerificationInput
  readonly candidate: FrozenDeliveryCandidate
  readonly evidenceId: EvidenceIdentifier | string
  readonly stageRunId: StageRunIdentifier | string
  readonly sessionBindingId: SessionBindingIdentifier | string
  readonly source: DeliveryEvidenceSource
  readonly runtimeEvents: readonly RuntimeEvent[]
  readonly createdAtMillis: number
}

/** Resolved metadata stays derived; the canonical persisted value is `evidence`. */
export interface ResolvedDeliveryEvidence {
  readonly schemaVersion: typeof DELIVERY_CANDIDATE_EVIDENCE_SCHEMA_VERSION
  readonly evidence: EvidenceRef
  readonly outcome: DeliveryRuntimeEvidenceOutcome
  readonly eventId: string | null
}

function evidenceError(
  code: DeliveryCandidateEvidenceErrorCode,
  message: string,
  options?: ErrorOptions,
): never {
  throw new DeliveryCandidateEvidenceError(code, message, options)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  keys: readonly string[],
  code: DeliveryCandidateEvidenceErrorCode,
  label: string,
): void {
  const expected = new Set(keys)
  if (Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))) {
    return evidenceError(code, `${label} has an unexpected shape`)
  }
}

function immutable<Value>(value: Value): Value {
  const clone = structuredClone(value)
  const pending: object[] = []
  if (typeof clone === 'object' && clone !== null) pending.push(clone)
  while (pending.length > 0) {
    const current = pending.pop()!
    if (Object.isFrozen(current)) continue
    Object.freeze(current)
    for (const child of Object.values(current)) {
      if (typeof child === 'object' && child !== null) pending.push(child)
    }
  }
  return clone
}

function parsedDelivery(value: Delivery): Delivery {
  try {
    return parseDelivery(value)
  } catch (error) {
    return evidenceError(
      'INVALID_DELIVERY',
      'candidate evidence requires a valid Delivery',
      { cause: error },
    )
  }
}

function gitObject(value: unknown, label: string): string {
  if (typeof value !== 'string' || !GIT_OBJECT_PATTERN.test(value)) {
    return evidenceError('INVALID_CANDIDATE', `${label} must be a lowercase Git object id`)
  }
  return value
}

function identifier<Identifier>(
  value: unknown,
  factory: (input: string) => Identifier,
  code: DeliveryCandidateEvidenceErrorCode,
  label: string,
): Identifier {
  try {
    if (typeof value !== 'string') throw new Error(`${label} must be a string`)
    return factory(value)
  } catch (error) {
    return evidenceError(code, `${label} is invalid`, { cause: error })
  }
}

function sha256(value: unknown, label: string): string {
  if (typeof value !== 'string' || !SHA256_PATTERN.test(value)) {
    return evidenceError('INVALID_CANDIDATE', `${label} must be a lowercase SHA-256 digest`)
  }
  return value
}

function portablePath(
  value: unknown,
  label: string,
  code: DeliveryCandidateEvidenceErrorCode = 'INVALID_CANDIDATE',
): string {
  if (typeof value !== 'string'
    || value.length === 0
    || value.length > MAX_PATH_LENGTH
    || value.startsWith('/')
    || value.includes('\\')
    || /^[A-Za-z]:/u.test(value)
    || /[\u0000-\u001f\u007f]/u.test(value)) {
    return evidenceError(code, `${label} must be a portable relative path`)
  }
  const segments = value.split('/')
  if (segments.some(segment => segment.length === 0 || segment === '.' || segment === '..')) {
    return evidenceError(code, `${label} contains an invalid path segment`)
  }
  return value
}

function candidatePathFacts(value: unknown): readonly DeliveryCandidatePathFact[] {
  if (!Array.isArray(value) || value.length > 100_000) {
    return evidenceError('INVALID_CANDIDATE', 'candidate changedPaths must be a bounded array')
  }
  const paths = value.map((entry, index) => {
    if (!isRecord(entry)
      || Object.keys(entry).length !== 3
      || !Object.hasOwn(entry, 'path')
      || !Object.hasOwn(entry, 'state')
      || !Object.hasOwn(entry, 'objectId')
      || (entry.state !== 'present' && entry.state !== 'deleted')) {
      return evidenceError(
        'INVALID_CANDIDATE',
        `candidate changedPaths[${String(index)}] has an unexpected shape`,
      )
    }
    const path = portablePath(entry.path, `candidate changedPaths[${String(index)}].path`)
    const objectId = entry.objectId === null
      ? null
      : gitObject(entry.objectId, `candidate changedPaths[${String(index)}].objectId`)
    if ((entry.state === 'present') !== (objectId !== null)) {
      return evidenceError(
        'INVALID_CANDIDATE',
        `candidate changedPaths[${String(index)}] state does not match its object id`,
      )
    }
    return Object.freeze({ path, state: entry.state, objectId })
  }).sort((left, right) => left.path < right.path ? -1 : left.path > right.path ? 1 : 0)
  if (new Set(paths.map(entry => entry.path)).size !== paths.length) {
    return evidenceError('INVALID_CANDIDATE', 'candidate changedPaths contains duplicate paths')
  }
  return Object.freeze(paths)
}

function roleId(value: string): StrongFlowRoleId | null {
  return STRONGFLOW_ROLE_IDS.includes(value as StrongFlowRoleId)
    ? value as StrongFlowRoleId
    : null
}

function candidateProducer(
  delivery: Delivery,
  producerStageRunId: StageRunIdentifier,
): StageRun {
  const run = delivery.stageRuns.find(entry => entry.id === producerStageRunId)
  if (run === undefined
    || run.actorType !== 'codex'
    || run.status !== 'succeeded'
    || (run.stage !== 'executing' && run.stage !== 'reworking')) {
    return evidenceError(
      'CANDIDATE_STALE',
      'candidate producer is not one completed execution or rework StageRun',
    )
  }
  const canonicalRole = roleId(run.role)
  if (canonicalRole === null
    || strongFlowRoleWorkspaceMode(canonicalRole) !== 'candidate-write') {
    return evidenceError(
      'INVALID_CANDIDATE',
      'candidate producer does not have the canonical candidate-write role policy',
    )
  }
  const producerIndex = delivery.stageRuns.findIndex(entry => entry.id === run.id)
  if (delivery.stageRuns.some((entry, index) => (
    entry.id !== run.id
    && entry.actorType === 'codex'
    && (entry.stage === 'executing' || entry.stage === 'reworking')
    && (index > producerIndex
      || entry.startedAtMillis > run.startedAtMillis
      || (entry.startedAtMillis === run.startedAtMillis && entry.attempt > run.attempt))
  ))) {
    return evidenceError(
      'CANDIDATE_STALE',
      'a later execution or rework StageRun may have changed this candidate',
    )
  }
  return run
}

function candidateProducerBinding(
  delivery: Delivery,
  run: StageRun,
  bindingId: SessionBindingIdentifier,
): SessionBinding {
  const binding = delivery.sessionBindings.find(entry => entry.id === bindingId)
  if (binding === undefined
    || binding.deliveryId !== delivery.id
    || binding.stageRunId !== run.id
    || binding.codexSessionId === null
    || binding.boundAtMillis < run.startedAtMillis
    || binding.boundAtMillis > run.finishedAtMillis!) {
    return evidenceError(
      'INVALID_CANDIDATE',
      'candidate producer does not match its exact Codex SessionBinding',
    )
  }
  return binding
}

function candidateIdentity(
  value: Omit<FrozenDeliveryCandidate, 'candidateRef'>,
): string {
  return `git-candidate:sha256:${createHash('sha256').update(JSON.stringify(value)).digest('hex')}`
}

/** Bind already-frozen Git commit/tree/diff facts to the current Delivery producer. */
export function freezeDeliveryCandidate(
  deliveryValue: Delivery,
  input: FreezeDeliveryCandidateInput,
): FrozenDeliveryCandidate {
  const delivery = parsedDelivery(deliveryValue)
  if (!isRecord(input)) {
    return evidenceError('INVALID_CANDIDATE', 'candidate freeze input must be an object')
  }
  exactKeys(input, [
    'producerStageRunId',
    'producerSessionBindingId',
    'baseCommitId',
    'baseTreeId',
    'candidateCommitId',
    'candidateTreeId',
    'diffSha256',
    'changedPaths',
  ], 'INVALID_CANDIDATE', 'candidate freeze input')
  const producerStageRunId = identifier(
    input.producerStageRunId,
    StageRunId,
    'INVALID_CANDIDATE',
    'candidate producerStageRunId',
  )
  const producerSessionBindingId = identifier(
    input.producerSessionBindingId,
    SessionBindingId,
    'INVALID_CANDIDATE',
    'candidate producerSessionBindingId',
  )
  const producer = candidateProducer(delivery, producerStageRunId)
  candidateProducerBinding(delivery, producer, producerSessionBindingId)
  const baseCommitId = gitObject(input.baseCommitId, 'candidate baseCommitId')
  const baseTreeId = gitObject(input.baseTreeId, 'candidate baseTreeId')
  const candidateCommitId = gitObject(input.candidateCommitId, 'candidate candidateCommitId')
  const candidateTreeId = gitObject(input.candidateTreeId, 'candidate candidateTreeId')
  const changedPaths = candidatePathFacts(input.changedPaths)
  const objectIds = [
    baseCommitId,
    baseTreeId,
    candidateCommitId,
    candidateTreeId,
    ...changedPaths.flatMap(entry => entry.objectId === null ? [] : [entry.objectId]),
  ]
  if (new Set(objectIds.map(value => value.length)).size !== 1) {
    return evidenceError(
      'INVALID_CANDIDATE',
      'candidate Git object ids must use one repository object format',
    )
  }
  if (GIT_OBJECT_PATTERN.test(delivery.spec.baseRevision)
    && delivery.spec.baseRevision !== baseCommitId) {
    return evidenceError(
      'INVALID_CANDIDATE',
      'candidate base commit does not match the DeliverySpec base revision',
    )
  }
  const unsigned: Omit<FrozenDeliveryCandidate, 'candidateRef'> = Object.freeze({
    schemaVersion: DELIVERY_CANDIDATE_EVIDENCE_SCHEMA_VERSION,
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    deliverySpecRevision: delivery.spec.revision,
    repositoryKind: delivery.spec.repository.kind,
    repositoryLocator: delivery.spec.repository.locator,
    baseRevision: delivery.spec.baseRevision,
    producerStageRunId,
    producerSessionBindingId,
    baseCommitId,
    baseTreeId,
    candidateCommitId,
    candidateTreeId,
    diffSha256: sha256(input.diffSha256, 'candidate diffSha256'),
    changedPaths,
  })
  return immutable({ ...unsigned, candidateRef: candidateIdentity(unsigned) })
}

/** Reject candidate facts changed in memory or superseded by a later writer StageRun. */
export function assertFrozenDeliveryCandidateCurrent(
  deliveryValue: Delivery,
  candidateValue: FrozenDeliveryCandidate,
): FrozenDeliveryCandidate {
  const delivery = parsedDelivery(deliveryValue)
  let candidate: FrozenDeliveryCandidate
  try {
    candidate = parseFrozenDeliveryCandidate(candidateValue, 'frozen candidate')
  } catch (error) {
    return evidenceError(
      'CANDIDATE_STALE',
      'frozen candidate is malformed',
      { cause: error },
    )
  }
  if (!CANDIDATE_REF_PATTERN.test(candidate.candidateRef)
    || candidate.deliveryId !== delivery.id
    || candidate.deliverySpecId !== delivery.spec.id
    || candidate.deliverySpecRevision !== delivery.spec.revision
    || candidate.repositoryKind !== delivery.spec.repository.kind
    || candidate.repositoryLocator !== delivery.spec.repository.locator
    || candidate.baseRevision !== delivery.spec.baseRevision) {
    return evidenceError(
      'CANDIDATE_STALE',
      'frozen candidate does not match the current DeliverySpec',
    )
  }
  let current: FrozenDeliveryCandidate
  try {
    current = freezeDeliveryCandidate(delivery, {
      producerStageRunId: candidate.producerStageRunId,
      producerSessionBindingId: candidate.producerSessionBindingId,
      baseCommitId: candidate.baseCommitId,
      baseTreeId: candidate.baseTreeId,
      candidateCommitId: candidate.candidateCommitId,
      candidateTreeId: candidate.candidateTreeId,
      diffSha256: candidate.diffSha256,
      changedPaths: candidate.changedPaths,
    })
  } catch (error) {
    if (error instanceof DeliveryCandidateEvidenceError
      && error.code !== 'INVALID_DELIVERY') {
      return evidenceError(
        'CANDIDATE_STALE',
        'the producer behind this frozen candidate is no longer current',
        { cause: error },
      )
    }
    throw error
  }
  if (candidate.candidateRef !== current.candidateRef) {
    return evidenceError(
      'CANDIDATE_STALE',
      'frozen candidate facts changed after their identity was computed',
    )
  }
  return current
}

function currentAcceptance(
  delivery: Delivery,
  input: AcceptanceVerificationInput,
): AcceptanceVerificationInput {
  try {
    return assertAcceptanceVerificationInputCurrent(delivery, input)
  } catch (error) {
    if (error instanceof AcceptanceVerificationError) {
      return evidenceError(
        'ACCEPTANCE_STALE',
        'candidate evidence does not use the current approved acceptance input',
        { cause: error },
      )
    }
    throw error
  }
}

function evidenceStageAndBinding(
  delivery: Delivery,
  candidate: FrozenDeliveryCandidate,
  stageRunIdValue: StageRunIdentifier | string,
  sessionBindingIdValue: SessionBindingIdentifier | string,
): { readonly stageRun: StageRun; readonly binding: SessionBinding } {
  const stageRunId = identifier(
    stageRunIdValue,
    StageRunId,
    'EVIDENCE_STAGE_MISMATCH',
    'evidence stageRunId',
  )
  const sessionBindingId = identifier(
    sessionBindingIdValue,
    SessionBindingId,
    'EVIDENCE_SESSION_MISMATCH',
    'evidence sessionBindingId',
  )
  const stageRun = delivery.stageRuns.find(entry => entry.id === stageRunId)
  if (stageRun === undefined || stageRun.actorType !== 'codex') {
    return evidenceError(
      'EVIDENCE_STAGE_MISMATCH',
      'evidence StageRun is missing or is not owned by Codex',
    )
  }
  const binding = delivery.sessionBindings.find(entry => entry.id === sessionBindingId)
  if (binding === undefined
    || binding.deliveryId !== delivery.id
    || binding.stageRunId !== stageRun.id
    || binding.codexSessionId === null
    || binding.boundAtMillis < stageRun.startedAtMillis) {
    return evidenceError(
      'EVIDENCE_SESSION_MISMATCH',
      'evidence does not match its exact Codex SessionBinding',
    )
  }
  const producer = delivery.stageRuns.find(entry => entry.id === candidate.producerStageRunId)!
  if (stageRun.id !== producer.id
    && (stageRun.stage !== 'verifying'
      || stageRun.deliveryTaskId !== producer.deliveryTaskId
      || stageRun.startedAtMillis < producer.finishedAtMillis!
      || binding.boundAtMillis < producer.finishedAtMillis!)) {
    return evidenceError(
      'EVIDENCE_CANDIDATE_MISMATCH',
      'evidence StageRun is not consuming the current producer candidate',
    )
  }
  return Object.freeze({ stageRun, binding })
}

const RUNTIME_EVIDENCE_TYPES = new Set<DeliveryRuntimeEvidenceType>([
  'test',
  'command',
  'diff',
  'file',
  'commit',
  'runtime_event',
  'review_finding',
])

function evidenceSource(value: unknown): DeliveryEvidenceSource {
  if (!isRecord(value) || typeof value.kind !== 'string') {
    return evidenceError('INVALID_EVIDENCE', 'evidence source must be an object with a kind')
  }
  switch (value.kind) {
    case 'runtime-event':
      exactKeys(value, ['kind', 'type', 'eventId'], 'INVALID_EVIDENCE', 'runtime evidence source')
      if (typeof value.type !== 'string'
        || !RUNTIME_EVIDENCE_TYPES.has(value.type as DeliveryRuntimeEvidenceType)
        || typeof value.eventId !== 'string'
        || value.eventId.length === 0) {
        return evidenceError('INVALID_EVIDENCE', 'runtime evidence source is invalid')
      }
      return Object.freeze({
        kind: 'runtime-event',
        type: value.type as DeliveryRuntimeEvidenceType,
        eventId: value.eventId,
      })
    case 'candidate-commit':
    case 'candidate-diff':
      exactKeys(value, ['kind'], 'INVALID_EVIDENCE', `${value.kind} evidence source`)
      return Object.freeze({ kind: value.kind })
    case 'candidate-file':
      exactKeys(value, ['kind', 'path'], 'INVALID_EVIDENCE', 'candidate-file evidence source')
      return Object.freeze({
        kind: 'candidate-file',
        path: portablePath(value.path, 'candidate file evidence path', 'INVALID_EVIDENCE'),
      })
    default:
      return evidenceError('INVALID_EVIDENCE', `evidence source kind ${value.kind} is unsupported`)
  }
}

function nestedRecord(
  value: Readonly<Record<string, unknown>>,
  key: string,
): Readonly<Record<string, unknown>> | undefined {
  return isRecord(value[key]) ? value[key] : undefined
}

function nestedString(
  event: RuntimeEvent,
  snakeCase: string,
  camelCase: string,
): string | undefined {
  const item = nestedRecord(event.data, 'item')
  const evidence = nestedRecord(event.data, 'evidence')
  for (const value of [
    evidence?.[snakeCase],
    evidence?.[camelCase],
    item?.[snakeCase],
    item?.[camelCase],
    event.data[snakeCase],
    event.data[camelCase],
  ]) {
    if (typeof value === 'string' && value.length > 0) return value
  }
  return undefined
}

function eventCandidateRef(event: RuntimeEvent): string | undefined {
  return event.semantic?.kind === 'verification-result'
    ? event.semantic.candidateRef
    : nestedString(event, 'candidate_ref', 'candidateRef')
}

function eventStatus(event: RuntimeEvent): string | undefined {
  const value = nestedString(event, 'status', 'status')
  return value?.toLowerCase().replaceAll('_', '-')
}

function successfulVerifierWrite(event: RuntimeEvent): boolean {
  const item = nestedRecord(event.data, 'item')
  const rawType = typeof event.data.type === 'string' ? event.data.type : ''
  const write = item?.type === 'FileChange'
    || rawType.startsWith('patch_apply_')
  if (!write) return false
  const status = eventStatus(event)
  return event.kind === 'tool.completed'
    && event.terminalReason !== 'declined'
    && event.terminalReason !== 'failed'
    && status !== 'sandbox-denied'
    && status !== 'policy-denied'
    && status !== 'declined'
    && status !== 'denied'
    && event.data.success !== false
    && item?.success !== false
}

function isReadOnlyCodexSessionConfiguration(event: RuntimeEvent): boolean {
  if (event.kind !== 'session.configured') return false
  const permissionProfile = nestedRecord(event.data, 'permission_profile')
  const fileSystem = permissionProfile === undefined
    ? undefined
    : nestedRecord(permissionProfile, 'file_system')
  const entries = fileSystem?.entries
  return event.data.approval_policy === 'on-request'
    && event.data.approvals_reviewer === 'user'
    && permissionProfile?.type === 'managed'
    && permissionProfile.network === 'restricted'
    && fileSystem?.type === 'restricted'
    && Array.isArray(entries)
    && entries.some(entry => isRecord(entry) && entry.access === 'read')
    && entries.every(entry => (
      isRecord(entry)
      && (entry.access === 'read' || entry.access === 'deny')
    ))
}

export function assertVerificationSessionReadOnly(
  stageRun: StageRun,
  binding: SessionBinding,
  events: readonly RuntimeEvent[],
): void {
  if (stageRun.stage !== 'verifying') return
  const canonicalRole = roleId(stageRun.role)
  if (canonicalRole !== 'reviewer'
    && canonicalRole !== 'verifier'
    && canonicalRole !== 'adversarial-verifier') {
    return evidenceError(
      'VERIFIER_POLICY_MISMATCH',
      `verification StageRun ${stageRun.id} does not use a canonical role`,
    )
  }
  if (strongFlowRoleWorkspaceMode(canonicalRole) !== 'candidate-read-only') {
    return evidenceError(
      'VERIFIER_POLICY_MISMATCH',
      `verification StageRun ${stageRun.id} does not use a read-only candidate policy`,
    )
  }
  const matching = events.filter(event => (
    isRecord(event)
    && isRecord(event.source)
    && isRecord(event.data)
    && event.source.sessionId === binding.dshSessionId
    && event.source.kernelSessionId === binding.codexSessionId
  ))
  if (matching.length === 0) return
  const configurations = matching.filter(event => event.kind === 'session.configured')
  if (configurations.length === 0 || configurations.some(event => (
    !isReadOnlyCodexSessionConfiguration(event)
  ))) {
    return evidenceError(
      'VERIFIER_POLICY_MISMATCH',
      `verification SessionBinding ${binding.id} lacks a read-only Codex permission profile`,
    )
  }
  if (matching.some(successfulVerifierWrite)) {
    return evidenceError(
      'VERIFIER_WRITE_OBSERVED',
      `verification SessionBinding ${binding.id} contains a successful candidate write`,
    )
  }
}

function runtimeEvidenceLink(
  delivery: Delivery,
  stageRun: StageRun,
  binding: SessionBinding,
  events: readonly RuntimeEvent[],
  source: Extract<DeliveryEvidenceSource, { readonly kind: 'runtime-event' }>,
): { readonly event: RuntimeEvent; readonly link: DeliveryRuntimeEvidenceLink } {
  const matches = events.filter(event => isRecord(event) && event.id === source.eventId)
  if (matches.length === 0) {
    return evidenceError('EVIDENCE_SOURCE_MISSING', `runtime event ${source.eventId} is missing`)
  }
  if (matches.length > 1) {
    return evidenceError(
      'EVIDENCE_SOURCE_AMBIGUOUS',
      `runtime event ${source.eventId} occurs more than once`,
    )
  }
  let projection: DeliveryRuntimeProjection
  try {
    projection = new DeliveryRuntimeProjection({ delivery })
    projection.replay(events)
  } catch (error) {
    if (error instanceof DeliveryRuntimeProjectionError) {
      return evidenceError(
        'EVIDENCE_SOURCE_MISSING',
        'runtime evidence could not be rebuilt from the supplied ledger facts',
        { cause: error },
      )
    }
    throw error
  }
  const session = projection.snapshot.stages
    .find(entry => entry.stageRun.id === stageRun.id)
    ?.sessions.find(entry => entry.binding.id === binding.id)
  const sourceRef = `runtime_event:${source.eventId}`
  const links = session?.evidenceLinks.filter(link => link.sourceRef === sourceRef) ?? []
  const link = links.find(entry => entry.type === source.type)
  if (link === undefined) {
    return evidenceError(
      links.length === 0 ? 'EVIDENCE_SOURCE_MISSING' : 'EVIDENCE_TYPE_MISMATCH',
      links.length === 0
        ? `runtime event ${source.eventId} is not an evidence fact`
        : `runtime event ${source.eventId} does not contain ${source.type} evidence`,
    )
  }
  const event = matches[0]!
  if (event.source.roleId !== stageRun.role
    || event.source.sessionId !== binding.dshSessionId
    || event.source.kernelSessionId !== binding.codexSessionId
    || link.stageRunId !== stageRun.id
    || link.sessionBindingId !== binding.id) {
    return evidenceError(
      'EVIDENCE_SESSION_MISMATCH',
      `runtime event ${source.eventId} belongs to another role, stage, or session`,
    )
  }
  return Object.freeze({ event, link })
}

function unifiedDiff(event: RuntimeEvent): string | undefined {
  return typeof event.data.unified_diff === 'string' ? event.data.unified_diff : undefined
}

function assertRuntimeCandidate(
  candidate: FrozenDeliveryCandidate,
  type: DeliveryRuntimeEvidenceType,
  event: RuntimeEvent,
): void {
  const referencedCandidate = eventCandidateRef(event)
  if (referencedCandidate !== undefined && referencedCandidate !== candidate.candidateRef) {
    return evidenceError(
      'EVIDENCE_CANDIDATE_MISMATCH',
      `runtime event ${event.id} names another frozen candidate`,
    )
  }
  if (type === 'diff') {
    const diff = unifiedDiff(event)
    const digest = diff === undefined
      ? undefined
      : createHash('sha256').update(diff).digest('hex')
    if (digest !== candidate.diffSha256) {
      return evidenceError(
        'EVIDENCE_CANDIDATE_MISMATCH',
        `runtime diff ${event.id} does not match the frozen candidate diff`,
      )
    }
  }
  if (type === 'commit') {
    const commitId = nestedString(event, 'candidate_commit_id', 'candidateCommitId')
    if (referencedCandidate === undefined || commitId !== candidate.candidateCommitId) {
      return evidenceError(
        'EVIDENCE_CANDIDATE_MISMATCH',
        `runtime commit ${event.id} does not identify the frozen candidate commit`,
      )
    }
  }
  if (type === 'file') {
    const path = nestedString(event, 'path', 'path')
    const objectId = nestedString(event, 'object_id', 'objectId')
    const fact = candidate.changedPaths.find(entry => entry.path === path)
    if (referencedCandidate === undefined
      || fact?.state !== 'present'
      || objectId !== fact.objectId) {
      return evidenceError(
        'EVIDENCE_CANDIDATE_MISMATCH',
        `runtime file ${event.id} does not identify a frozen candidate file`,
      )
    }
  }
  if (type === 'review_finding' && referencedCandidate === undefined) {
    return evidenceError(
      'EVIDENCE_CANDIDATE_MISMATCH',
      `review finding ${event.id} does not identify the frozen candidate`,
    )
  }
  if (type === 'review_finding'
    && (event.semantic?.kind !== 'verification-result'
      || event.semantic.deliverySpecId !== candidate.deliverySpecId
      || event.semantic.deliverySpecRevision !== candidate.deliverySpecRevision
      || event.semantic.candidateRef !== candidate.candidateRef)) {
    return evidenceError(
      'EVIDENCE_CANDIDATE_MISMATCH',
      `review finding ${event.id} does not identify the current spec and candidate`,
    )
  }
}

function sourceRefForCandidate(
  candidate: FrozenDeliveryCandidate,
  source: Exclude<DeliveryEvidenceSource, { readonly kind: 'runtime-event' }>,
): { readonly type: 'commit' | 'diff' | 'file'; readonly sourceRef: string } {
  if (source.kind === 'candidate-commit') {
    return Object.freeze({
      type: 'commit',
      sourceRef: `git_commit:${candidate.candidateCommitId}`,
    })
  }
  if (source.kind === 'candidate-diff') {
    return Object.freeze({
      type: 'diff',
      sourceRef: `git_diff:sha256:${candidate.diffSha256}`,
    })
  }
  const path = portablePath(source.path, 'candidate file evidence path')
  const fact = candidate.changedPaths.find(entry => entry.path === path)
  if (fact?.state !== 'present' || fact.objectId === null) {
    return evidenceError(
      'EVIDENCE_SOURCE_MISSING',
      `candidate file ${path} does not exist in the frozen changed-path facts`,
    )
  }
  return Object.freeze({
    type: 'file',
    sourceRef: `git_file:${candidate.candidateTreeId}:${encodeURIComponent(path)}@${fact.objectId}`,
  })
}

function createdAtMillis(value: unknown): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Object.is(value, -0)) {
    return evidenceError(
      'INVALID_EVIDENCE',
      'evidence createdAtMillis must be a non-negative safe integer',
    )
  }
  return Number(value)
}

/**
 * Resolve one canonical EvidenceRef from current candidate facts or an existing
 * Codex RuntimeEvent. No command, test, Agent, or Delivery mutation is started.
 */
export function resolveDeliveryEvidence(
  input: ResolveDeliveryEvidenceInput,
): ResolvedDeliveryEvidence {
  if (!isRecord(input) || !Array.isArray(input.runtimeEvents)) {
    return evidenceError('INVALID_EVIDENCE', 'evidence resolution input is malformed')
  }
  exactKeys(input, [
    'delivery',
    'acceptance',
    'candidate',
    'evidenceId',
    'stageRunId',
    'sessionBindingId',
    'source',
    'runtimeEvents',
    'createdAtMillis',
  ], 'INVALID_EVIDENCE', 'evidence resolution input')
  const source = evidenceSource(input.source)
  const delivery = parsedDelivery(input.delivery)
  currentAcceptance(delivery, input.acceptance)
  const candidate = assertFrozenDeliveryCandidateCurrent(delivery, input.candidate)
  const { stageRun, binding } = evidenceStageAndBinding(
    delivery,
    candidate,
    input.stageRunId,
    input.sessionBindingId,
  )
  assertVerificationSessionReadOnly(stageRun, binding, input.runtimeEvents)
  const evidenceId = identifier(
    input.evidenceId,
    EvidenceRefId,
    'INVALID_EVIDENCE',
    'evidence id',
  )
  const timestamp = createdAtMillis(input.createdAtMillis)
  if (timestamp < stageRun.startedAtMillis || timestamp < binding.boundAtMillis) {
    return evidenceError(
      'EVIDENCE_STAGE_MISMATCH',
      'evidence was bound before its StageRun started',
    )
  }

  let type: Exclude<EvidenceRefType, 'pull_request'>
  let sourceRef: string
  let outcome: DeliveryRuntimeEvidenceOutcome
  let eventId: string | null
  if (source.kind === 'runtime-event') {
    const resolved = runtimeEvidenceLink(
      delivery,
      stageRun,
      binding,
      input.runtimeEvents,
      source,
    )
    assertRuntimeCandidate(candidate, source.type, resolved.event)
    type = source.type
    sourceRef = resolved.link.sourceRef
    outcome = deliveryRuntimeEvidenceOutcome(resolved.event, type)
    if (outcome !== resolved.link.outcome) {
      return evidenceError(
        'EVIDENCE_TYPE_MISMATCH',
        `runtime event ${resolved.event.id} changed after semantic projection`,
      )
    }
    eventId = resolved.event.id
  } else {
    if (stageRun.id !== candidate.producerStageRunId
      || binding.id !== candidate.producerSessionBindingId) {
      return evidenceError(
        'EVIDENCE_SESSION_MISMATCH',
        'direct Git candidate facts must retain their producer StageRun and SessionBinding',
      )
    }
    const resolved = sourceRefForCandidate(candidate, source)
    type = resolved.type
    sourceRef = resolved.sourceRef
    outcome = 'observed'
    eventId = null
  }

  let evidence: EvidenceRef
  try {
    evidence = parseEvidenceRef({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: evidenceId,
      deliveryId: delivery.id,
      deliverySpecId: delivery.spec.id,
      deliverySpecRevision: delivery.spec.revision,
      stageRunId: stageRun.id,
      sessionBindingId: binding.id,
      candidateRef: candidate.candidateRef,
      type,
      sourceRef,
      createdAtMillis: timestamp,
    })
  } catch (error) {
    return evidenceError('INVALID_EVIDENCE', 'resolved EvidenceRef is invalid', { cause: error })
  }
  return immutable({
    schemaVersion: DELIVERY_CANDIDATE_EVIDENCE_SCHEMA_VERSION,
    evidence,
    outcome,
    eventId,
  })
}
