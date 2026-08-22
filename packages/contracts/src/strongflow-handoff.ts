import {
  AttemptId,
  DiagramId,
  JobId,
  RequirementId,
  SolutionId,
  StageRunId,
  type AttemptId as AttemptIdentifier,
  type DefinitionIdentity,
  type JobId as JobIdentifier,
  type StageRunId as StageRunIdentifier,
} from './strongflow-job.js'
import {
  STRONGFLOW_ARTIFACT_KINDS,
  parseStrongFlowCandidateIdentity,
  type StrongFlowArtifactKind,
} from './strongflow-artifact.js'
import {
  STRONGFLOW_ROLE_IDS,
  strongFlowRoleAcceptedInputArtifacts,
  type StrongFlowRoleId,
} from './strongflow-role.js'
import type { StrongFlowCandidateIdentity } from './strongflow-workspace.js'

export const STRONGFLOW_HANDOFF_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_HANDOFF_MAX_INPUTS = 32
export const STRONGFLOW_HANDOFF_MAX_CONTEXT_BYTES = 64 * 1024 * 1024

const PORTABLE_IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/-]{0,199}$/u
const DECIMAL_SEQUENCE_PATTERN = /^(?:0|[1-9][0-9]*)$/u
const BLOB_ID_PATTERN = /^sha256-[0-9a-f]{64}$/u
const RECORD_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u

declare const strongFlowHandoffIdentifierBrand: unique symbol

export type StrongFlowHandoffId = string & {
  readonly [strongFlowHandoffIdentifierBrand]: 'StrongFlowHandoffId'
}

export function StrongFlowHandoffId(value: string): StrongFlowHandoffId {
  if (!PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    throw new StrongFlowHandoffValidationError(
      'INVALID_HANDOFF_IDENTITY',
      'handoff.handoffId',
      'handoff id must be a portable identifier',
    )
  }
  return value as StrongFlowHandoffId
}

export type StrongFlowHandoffValidationErrorCode =
  | 'INVALID_HANDOFF_SHAPE'
  | 'UNSUPPORTED_SCHEMA_VERSION'
  | 'INVALID_HANDOFF_IDENTITY'
  | 'INPUT_SET_MISMATCH'
  | 'CONTEXT_LIMIT_EXCEEDED'
  | 'INVALID_RELATIONSHIP'

export class StrongFlowHandoffValidationError extends Error {
  readonly code: StrongFlowHandoffValidationErrorCode
  readonly path: string

  constructor(
    code: StrongFlowHandoffValidationErrorCode,
    path: string,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowHandoffValidationError'
    this.code = code
    this.path = path
  }
}

export type StrongFlowHandoffTarget =
  | {
    readonly kind: 'human-review'
  }
  | {
    readonly kind: 'role'
    readonly roleId: StrongFlowRoleId
    readonly stageRunId: StageRunIdentifier
    readonly attemptId: AttemptIdentifier
  }

export interface StrongFlowHandoffInputReference {
  readonly position: number
  readonly artifactKind: StrongFlowArtifactKind
  readonly artifactId: string
  readonly artifactRecordId: string
  readonly blobId: string
  readonly byteLength: number
}

export interface StrongFlowHandoffManifest {
  readonly schemaVersion: typeof STRONGFLOW_HANDOFF_SCHEMA_VERSION
  readonly handoffId: StrongFlowHandoffId
  readonly jobId: JobIdentifier
  readonly jobSequence: string
  readonly target: StrongFlowHandoffTarget
  readonly definition: DefinitionIdentity | null
  readonly candidate: StrongFlowCandidateIdentity | null
  readonly inputs: readonly StrongFlowHandoffInputReference[]
  readonly modelInputBytes: number
  readonly contextLimitBytes: number
  readonly producer: {
    readonly kind: 'system'
    readonly actorId: string
  }
  readonly createdAtMillis: number
}

export type StrongFlowHandoffManifestInput = Omit<
  StrongFlowHandoffManifest,
  'schemaVersion'
>

function handoffError(
  code: StrongFlowHandoffValidationErrorCode,
  path: string,
  message: string,
  options?: ErrorOptions,
): never {
  throw new StrongFlowHandoffValidationError(code, path, message, options)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (!isRecord(value)) {
    handoffError('INVALID_HANDOFF_SHAPE', path, `${path} must be an object`)
  }
  return value
}

function exactKeys(
  value: Record<string, unknown>,
  keys: readonly string[],
  path: string,
): void {
  const expected = new Set(keys)
  if (
    Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))
  ) handoffError('INVALID_HANDOFF_SHAPE', path, `${path} has an unexpected shape`)
}

function portableIdentifier(value: unknown, path: string): string {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    handoffError('INVALID_HANDOFF_IDENTITY', path, `${path} is not a portable identifier`)
  }
  return value
}

function nonNegativeInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0) {
    handoffError('INVALID_HANDOFF_SHAPE', path, `${path} must be a non-negative integer`)
  }
  return Number(value)
}

function positiveInteger(value: unknown, path: string): number {
  const result = nonNegativeInteger(value, path)
  if (result === 0) handoffError('INVALID_HANDOFF_SHAPE', path, `${path} must be positive`)
  return result
}

function sequence(value: unknown, path: string): string {
  if (typeof value !== 'string' || !DECIMAL_SEQUENCE_PATTERN.test(value) || value === '0') {
    handoffError('INVALID_HANDOFF_IDENTITY', path, `${path} must be a positive sequence`)
  }
  return value
}

function parseDefinition(value: unknown, path: string): DefinitionIdentity | null {
  if (value === null) return null
  const input = record(value, path)
  exactKeys(input, [
    'requirementId',
    'solutionId',
    'systemArchitectureDiagramId',
    'processFlowDiagramId',
  ], path)
  try {
    return Object.freeze({
      requirementId: RequirementId(String(input.requirementId)),
      solutionId: SolutionId(String(input.solutionId)),
      systemArchitectureDiagramId: DiagramId(String(input.systemArchitectureDiagramId)),
      processFlowDiagramId: DiagramId(String(input.processFlowDiagramId)),
    })
  } catch (error) {
    handoffError(
      'INVALID_HANDOFF_IDENTITY',
      path,
      `${path} contains an invalid definition identity`,
      { cause: error },
    )
  }
}

function parseTarget(value: unknown): StrongFlowHandoffTarget {
  const input = record(value, 'handoff.target')
  if (input.kind === 'human-review') {
    exactKeys(input, ['kind'], 'handoff.target')
    return Object.freeze({ kind: 'human-review' })
  }
  if (input.kind !== 'role') {
    handoffError('INVALID_HANDOFF_SHAPE', 'handoff.target.kind', 'handoff target is unsupported')
  }
  exactKeys(input, ['kind', 'roleId', 'stageRunId', 'attemptId'], 'handoff.target')
  if (typeof input.roleId !== 'string' || !STRONGFLOW_ROLE_IDS.includes(
    input.roleId as StrongFlowRoleId,
  )) handoffError('INVALID_HANDOFF_IDENTITY', 'handoff.target.roleId', 'handoff role is invalid')
  try {
    return Object.freeze({
      kind: 'role',
      roleId: input.roleId as StrongFlowRoleId,
      stageRunId: StageRunId(String(input.stageRunId)),
      attemptId: AttemptId(String(input.attemptId)),
    })
  } catch (error) {
    handoffError(
      'INVALID_HANDOFF_IDENTITY',
      'handoff.target',
      'handoff role run identity is invalid',
      { cause: error },
    )
  }
}

function parseInputs(value: unknown): readonly StrongFlowHandoffInputReference[] {
  if (!Array.isArray(value) || value.length === 0 || value.length > STRONGFLOW_HANDOFF_MAX_INPUTS) {
    handoffError(
      'INVALID_HANDOFF_SHAPE',
      'handoff.inputs',
      `handoff inputs must contain 1 through ${STRONGFLOW_HANDOFF_MAX_INPUTS} entries`,
    )
  }
  const artifactIds = new Set<string>()
  const recordIds = new Set<string>()
  return Object.freeze(value.map((entry, index) => {
    const input = record(entry, `handoff.inputs[${index}]`)
    exactKeys(input, [
      'position',
      'artifactKind',
      'artifactId',
      'artifactRecordId',
      'blobId',
      'byteLength',
    ], `handoff.inputs[${index}]`)
    if (input.position !== index) {
      handoffError(
        'INPUT_SET_MISMATCH',
        `handoff.inputs[${index}].position`,
        'handoff input positions must be consecutive and ordered',
      )
    }
    if (typeof input.artifactKind !== 'string' || !STRONGFLOW_ARTIFACT_KINDS.includes(
      input.artifactKind as StrongFlowArtifactKind,
    )) handoffError('INPUT_SET_MISMATCH', `handoff.inputs[${index}].artifactKind`, 'handoff artifact kind is invalid')
    const artifactId = portableIdentifier(input.artifactId, `handoff.inputs[${index}].artifactId`)
    if (artifactIds.has(artifactId)) {
      handoffError('INPUT_SET_MISMATCH', `handoff.inputs[${index}].artifactId`, 'handoff artifact identity is repeated')
    }
    artifactIds.add(artifactId)
    if (typeof input.artifactRecordId !== 'string'
      || !RECORD_ID_PATTERN.test(input.artifactRecordId)
      || recordIds.has(input.artifactRecordId)) {
      handoffError('INPUT_SET_MISMATCH', `handoff.inputs[${index}].artifactRecordId`, 'handoff record identity is invalid or repeated')
    }
    recordIds.add(input.artifactRecordId)
    if (typeof input.blobId !== 'string' || !BLOB_ID_PATTERN.test(input.blobId)) {
      handoffError('INPUT_SET_MISMATCH', `handoff.inputs[${index}].blobId`, 'handoff blob identity is invalid')
    }
    return Object.freeze({
      position: index,
      artifactKind: input.artifactKind as StrongFlowArtifactKind,
      artifactId,
      artifactRecordId: input.artifactRecordId,
      blobId: input.blobId,
      byteLength: positiveInteger(input.byteLength, `handoff.inputs[${index}].byteLength`),
    })
  }))
}

function expectedKinds(target: StrongFlowHandoffTarget): readonly StrongFlowArtifactKind[] {
  return target.kind === 'human-review'
    ? Object.freeze([
      'REQUIREMENT_SPEC',
      'SOLUTION_DESIGN',
      'SYSTEM_ARCHITECTURE_DIAGRAM',
      'PROCESS_FLOW_DIAGRAM',
    ])
    : strongFlowRoleAcceptedInputArtifacts(target.roleId)
}

function validateInputKinds(
  target: StrongFlowHandoffTarget,
  inputs: readonly StrongFlowHandoffInputReference[],
): void {
  const expected = expectedKinds(target)
  if (
    inputs.length !== expected.length
    || expected.some((kind, index) => inputs[index]?.artifactKind !== kind)
  ) handoffError('INPUT_SET_MISMATCH', 'handoff.inputs', 'handoff inputs do not match the target contract')
}

function validateDefinition(
  target: StrongFlowHandoffTarget,
  definition: DefinitionIdentity | null,
  inputs: readonly StrongFlowHandoffInputReference[],
): void {
  const requiresDefinition = target.kind === 'human-review'
    || (target.kind === 'role' && !['requirements', 'solution'].includes(target.roleId))
  if (requiresDefinition !== (definition !== null)) {
    handoffError('INVALID_RELATIONSHIP', 'handoff.definition', 'handoff definition presence does not match its target')
  }
  if (definition === null) return
  const ids = new Map(inputs.map(input => [input.artifactKind, input.artifactId]))
  const hasDefinitionInputs = [
    'REQUIREMENT_SPEC',
    'SOLUTION_DESIGN',
    'SYSTEM_ARCHITECTURE_DIAGRAM',
    'PROCESS_FLOW_DIAGRAM',
  ].some(kind => ids.has(kind as StrongFlowArtifactKind))
  if (!hasDefinitionInputs
    && target.kind === 'role'
    && target.roleId === 'executor'
    && ids.has('EXECUTION_PLAN')) return
  if (
    ids.get('REQUIREMENT_SPEC') !== definition.requirementId
    || ids.get('SOLUTION_DESIGN') !== definition.solutionId
    || ids.get('SYSTEM_ARCHITECTURE_DIAGRAM') !== definition.systemArchitectureDiagramId
    || ids.get('PROCESS_FLOW_DIAGRAM') !== definition.processFlowDiagramId
  ) handoffError('INVALID_RELATIONSHIP', 'handoff.definition', 'handoff definition does not match its inputs')
}

function validateCandidate(
  target: StrongFlowHandoffTarget,
  candidate: StrongFlowCandidateIdentity | null,
): void {
  const requiresCandidate = target.kind === 'role'
    && ['reviewer', 'verifier', 'adversarial-verifier', 'remediator'].includes(target.roleId)
  if (requiresCandidate !== (candidate !== null)) {
    handoffError('INVALID_RELATIONSHIP', 'handoff.candidate', 'handoff candidate presence does not match its target')
  }
}

export function parseStrongFlowHandoffManifest(value: unknown): StrongFlowHandoffManifest {
  const input = record(value, 'handoff')
  exactKeys(input, [
    'schemaVersion',
    'handoffId',
    'jobId',
    'jobSequence',
    'target',
    'definition',
    'candidate',
    'inputs',
    'modelInputBytes',
    'contextLimitBytes',
    'producer',
    'createdAtMillis',
  ], 'handoff')
  if (input.schemaVersion !== STRONGFLOW_HANDOFF_SCHEMA_VERSION) {
    handoffError('UNSUPPORTED_SCHEMA_VERSION', 'handoff.schemaVersion', 'handoff schema version is unsupported')
  }
  let jobId: JobIdentifier
  try {
    jobId = JobId(String(input.jobId))
  } catch (error) {
    handoffError('INVALID_HANDOFF_IDENTITY', 'handoff.jobId', 'handoff job id is invalid', { cause: error })
  }
  const target = parseTarget(input.target)
  const inputs = parseInputs(input.inputs)
  validateInputKinds(target, inputs)
  const definition = parseDefinition(input.definition, 'handoff.definition')
  validateDefinition(target, definition, inputs)
  const candidate = input.candidate === null
    ? null
    : parseStrongFlowCandidateIdentity(input.candidate, 'handoff.candidate')
  validateCandidate(target, candidate)
  const contextLimitBytes = positiveInteger(input.contextLimitBytes, 'handoff.contextLimitBytes')
  if (contextLimitBytes > STRONGFLOW_HANDOFF_MAX_CONTEXT_BYTES) {
    handoffError('CONTEXT_LIMIT_EXCEEDED', 'handoff.contextLimitBytes', 'handoff context limit exceeds the product maximum')
  }
  const modelInputBytes = positiveInteger(input.modelInputBytes, 'handoff.modelInputBytes')
  if (modelInputBytes !== inputs.reduce((total, entry) => total + entry.byteLength, 0)) {
    handoffError(
      'INVALID_RELATIONSHIP',
      'handoff.modelInputBytes',
      'handoff model input bytes do not match the referenced artifact content',
    )
  }
  if (modelInputBytes > contextLimitBytes) {
    handoffError('CONTEXT_LIMIT_EXCEEDED', 'handoff.modelInputBytes', 'handoff model input exceeds its context limit')
  }
  const producer = record(input.producer, 'handoff.producer')
  exactKeys(producer, ['kind', 'actorId'], 'handoff.producer')
  if (producer.kind !== 'system') {
    handoffError('INVALID_RELATIONSHIP', 'handoff.producer.kind', 'handoff producer must be the program')
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_HANDOFF_SCHEMA_VERSION,
    handoffId: StrongFlowHandoffId(String(input.handoffId)),
    jobId,
    jobSequence: sequence(input.jobSequence, 'handoff.jobSequence'),
    target,
    definition,
    candidate,
    inputs,
    modelInputBytes,
    contextLimitBytes,
    producer: Object.freeze({
      kind: 'system',
      actorId: portableIdentifier(producer.actorId, 'handoff.producer.actorId'),
    }),
    createdAtMillis: nonNegativeInteger(input.createdAtMillis, 'handoff.createdAtMillis'),
  })
}

export function materializeStrongFlowHandoffManifest(
  input: StrongFlowHandoffManifestInput,
): StrongFlowHandoffManifest {
  return parseStrongFlowHandoffManifest({
    schemaVersion: STRONGFLOW_HANDOFF_SCHEMA_VERSION,
    ...input,
  })
}
