import type {
  AttemptId,
  CandidateId,
  DefinitionIdentity,
  DefinitionRevisionScope,
  DiagramId,
  HumanReviewChannel,
  HumanReviewId,
  JobId,
  KernelSessionId,
  RequirementId,
  SolutionId,
  StageRunId,
} from './strongflow-job.js'
import {
  STRONGFLOW_ROLE_ARTIFACT_KINDS,
  STRONGFLOW_ROLE_IDS,
  type StrongFlowRoleArtifactKind,
  type StrongFlowRoleId,
} from './strongflow-role.js'
import type {
  GitCommitId,
  GitDiffId,
  GitTreeId,
  SourceSnapshotId,
  StrongFlowCandidateIdentity,
} from './strongflow-workspace.js'

/** Canonical version for every persisted or transported StrongFlow artifact. */
export const STRONGFLOW_ARTIFACT_SCHEMA_VERSION = 1 as const

export const STRONGFLOW_ARTIFACT_KINDS = Object.freeze([
  ...STRONGFLOW_ROLE_ARTIFACT_KINDS,
  'EXECUTION_CHANGE_ANNOTATION',
] as const)

export type StrongFlowArtifactKind = typeof STRONGFLOW_ARTIFACT_KINDS[number]

declare const strongFlowArtifactIdentifierBrand: unique symbol

type StrongFlowArtifactIdentifier<Name extends string> = string & {
  readonly [strongFlowArtifactIdentifierBrand]: Name
}

export type UserRequestId = StrongFlowArtifactIdentifier<'UserRequestId'>
export type ExecutionPlanId = StrongFlowArtifactIdentifier<'ExecutionPlanId'>
export type PatchManifestId = StrongFlowArtifactIdentifier<'PatchManifestId'>
export type ReviewReportId = StrongFlowArtifactIdentifier<'ReviewReportId'>
export type VerificationReportId = StrongFlowArtifactIdentifier<'VerificationReportId'>
export type RemediationRequestId = StrongFlowArtifactIdentifier<'RemediationRequestId'>
export type RemediationReportId = StrongFlowArtifactIdentifier<'RemediationReportId'>
export type DeliveryReceiptId = StrongFlowArtifactIdentifier<'DeliveryReceiptId'>
export type ExecutionChangeAnnotationId = StrongFlowArtifactIdentifier<
  'ExecutionChangeAnnotationId'
>
export type DiagramNodeId = StrongFlowArtifactIdentifier<'DiagramNodeId'>

const PORTABLE_IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/-]{0,199}$/u
const DECIMAL_SEQUENCE_PATTERN = /^(?:0|[1-9][0-9]*)$/u
const SHA256_PATTERN = /^[0-9a-f]{64}$/u
const GIT_OBJECT_PATTERN = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u
const SOURCE_SNAPSHOT_PATTERN = /^source-sha256-[0-9a-f]{64}$/u
const MAX_TEXT_LENGTH = 1_048_576

export type StrongFlowArtifactValidationErrorCode =
  | 'INVALID_ARTIFACT_SHAPE'
  | 'UNSUPPORTED_SCHEMA_VERSION'
  | 'UNKNOWN_ARTIFACT_KIND'
  | 'ARTIFACT_KIND_MISMATCH'
  | 'INVALID_IDENTIFIER'
  | 'INVALID_VALUE'
  | 'INVALID_SOURCE_ARTIFACTS'
  | 'INVALID_PRODUCER'
  | 'INVALID_EVENT_INTERVAL'
  | 'INVALID_RELATIONSHIP'
  | 'SOURCE_INPUT_MISMATCH'
  | 'STALE_ANNOTATION_TARGET'

/** Stable boundary error shared by model, native, storage, UI, and command callers. */
export class StrongFlowArtifactValidationError extends Error {
  readonly code: StrongFlowArtifactValidationErrorCode
  readonly path: string

  constructor(
    code: StrongFlowArtifactValidationErrorCode,
    path: string,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowArtifactValidationError'
    this.code = code
    this.path = path
  }
}

function artifactError(
  code: StrongFlowArtifactValidationErrorCode,
  path: string,
  message: string,
  options?: ErrorOptions,
): never {
  throw new StrongFlowArtifactValidationError(code, path, message, options)
}

function artifactIdentifier<Name extends string>(
  value: string,
  name: Name,
): StrongFlowArtifactIdentifier<Name> {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    artifactError(
      'INVALID_IDENTIFIER',
      name,
      `${name} must be a portable identifier of at most 200 characters`,
    )
  }
  return value as StrongFlowArtifactIdentifier<Name>
}

export function UserRequestId(value: string): UserRequestId {
  return artifactIdentifier(value, 'UserRequestId')
}

export function ExecutionPlanId(value: string): ExecutionPlanId {
  return artifactIdentifier(value, 'ExecutionPlanId')
}

export function PatchManifestId(value: string): PatchManifestId {
  return artifactIdentifier(value, 'PatchManifestId')
}

export function ReviewReportId(value: string): ReviewReportId {
  return artifactIdentifier(value, 'ReviewReportId')
}

export function VerificationReportId(value: string): VerificationReportId {
  return artifactIdentifier(value, 'VerificationReportId')
}

export function RemediationRequestId(value: string): RemediationRequestId {
  return artifactIdentifier(value, 'RemediationRequestId')
}

export function RemediationReportId(value: string): RemediationReportId {
  return artifactIdentifier(value, 'RemediationReportId')
}

export function DeliveryReceiptId(value: string): DeliveryReceiptId {
  return artifactIdentifier(value, 'DeliveryReceiptId')
}

export function ExecutionChangeAnnotationId(value: string): ExecutionChangeAnnotationId {
  return artifactIdentifier(value, 'ExecutionChangeAnnotationId')
}

export function DiagramNodeId(value: string): DiagramNodeId {
  return artifactIdentifier(value, 'DiagramNodeId')
}

export interface StrongFlowArtifactReference<
  Kind extends StrongFlowArtifactKind = StrongFlowArtifactKind,
> {
  readonly artifactKind: Kind
  readonly artifactId: string
}

export type StrongFlowArtifactProducer =
  | {
    readonly kind: 'role'
    readonly roleId: StrongFlowRoleId
    readonly stageRunId: StageRunId
    readonly attemptId: AttemptId
  }
  | {
    readonly kind: 'human'
    readonly actorId: string
    readonly channel: HumanReviewChannel
  }
  | {
    readonly kind: 'system'
    readonly actorId: string
  }

export interface StrongFlowArtifactKernelEventInterval {
  readonly schemaVersion: 1
  readonly kernelSessionLineageId: string
  readonly contextId: string
  readonly generation: number
  readonly kernelSessionId: KernelSessionId
  readonly kernelStreamId: string
  readonly turnId: string
  readonly firstSequence: string
  readonly lastSequence: string
  readonly eventCount: number
}

export interface StrongFlowArtifactStatement {
  readonly id: string
  readonly text: string
}

export interface StrongFlowArtifactRisk {
  readonly riskId: string
  readonly statement: string
  readonly mitigation: string | null
}

export interface UserRequestPayload {
  readonly request: string
  readonly submittedFrom: 'chat' | 'strongflow-workbench' | 'cli'
}

export interface RequirementAcceptanceCriterion {
  readonly criterionId: string
  readonly statement: string
  readonly verification: string
}

export interface RequirementRepositoryFact {
  readonly factId: string
  readonly statement: string
  readonly evidence: string
}

export interface RequirementOpenQuestion {
  readonly questionId: string
  readonly question: string
  readonly blocking: boolean
}

export interface RequirementSpecPayload {
  readonly title: string
  readonly summary: string
  readonly goals: readonly StrongFlowArtifactStatement[]
  readonly nonGoals: readonly StrongFlowArtifactStatement[]
  readonly constraints: readonly StrongFlowArtifactStatement[]
  readonly acceptanceCriteria: readonly RequirementAcceptanceCriterion[]
  readonly repositoryFacts: readonly RequirementRepositoryFact[]
  readonly risks: readonly StrongFlowArtifactRisk[]
  readonly openQuestions: readonly RequirementOpenQuestion[]
}

export interface SolutionDecision {
  readonly decisionId: string
  readonly title: string
  readonly decision: string
  readonly rationale: string
  readonly requirementItemIds: readonly string[]
}

export interface SolutionComponent {
  readonly componentId: string
  readonly name: string
  readonly kind: 'surface' | 'service' | 'module' | 'store' | 'external'
  readonly responsibility: string
  readonly trustBoundary: string | null
  readonly sourcePaths: readonly string[]
}

export interface SolutionConnection {
  readonly connectionId: string
  readonly fromComponentId: string
  readonly toComponentId: string
  readonly label: string
}

export interface SolutionUnresolvedFact {
  readonly factId: string
  readonly question: string
  readonly impact: string
}

export interface SolutionDesignPayload {
  readonly requirementId: RequirementId
  readonly summary: string
  readonly decisions: readonly SolutionDecision[]
  readonly components: readonly SolutionComponent[]
  readonly connections: readonly SolutionConnection[]
  readonly unresolvedFacts: readonly SolutionUnresolvedFact[]
  readonly risks: readonly StrongFlowArtifactRisk[]
}

export interface SystemArchitectureDiagramNode {
  readonly nodeId: DiagramNodeId
  readonly label: string
  readonly kind:
    | 'actor'
    | 'surface'
    | 'service'
    | 'module'
    | 'store'
    | 'external'
    | 'boundary'
    | 'unresolved'
  readonly description: string
  readonly trustBoundary: string | null
  readonly unresolved: boolean
  readonly componentIds: readonly string[]
  readonly sourcePaths: readonly string[]
}

export interface ProcessFlowDiagramNode {
  readonly nodeId: DiagramNodeId
  readonly label: string
  readonly kind: 'start' | 'stage' | 'human-review' | 'decision' | 'state' | 'end'
  readonly description: string
  readonly roleId: StrongFlowRoleId | null
  readonly unresolved: boolean
}

export interface StrongFlowDiagramEdge {
  readonly edgeId: string
  readonly fromNodeId: DiagramNodeId
  readonly toNodeId: DiagramNodeId
  readonly label: string
}

interface StrongFlowDiagramPayloadBase {
  readonly requirementId: RequirementId
  readonly solutionId: SolutionId
  readonly title: string
  readonly edges: readonly StrongFlowDiagramEdge[]
}

export interface SystemArchitectureDiagramPayload extends StrongFlowDiagramPayloadBase {
  readonly nodes: readonly SystemArchitectureDiagramNode[]
}

export interface ProcessFlowDiagramPayload extends StrongFlowDiagramPayloadBase {
  readonly nodes: readonly ProcessFlowDiagramNode[]
}

export interface HumanReviewRecordPayload {
  readonly definition: DefinitionIdentity
  readonly decision: 'approved' | 'changes-requested' | 'rejected'
  readonly comment: string | null
  readonly scope: DefinitionRevisionScope | null
}

export interface ExecutionPlanStep {
  readonly stepId: string
  readonly title: string
  readonly instructions: string
  readonly dependsOn: readonly string[]
  readonly paths: readonly string[]
  readonly commands: readonly string[]
  readonly checks: readonly string[]
}

export interface ExecutionPlanPayload {
  readonly definition: DefinitionIdentity
  readonly humanReviewId: HumanReviewId
  readonly summary: string
  readonly steps: readonly ExecutionPlanStep[]
}

export interface PatchHunkRecord {
  readonly hunkId: string
  readonly oldStart: number
  readonly oldLines: number
  readonly newStart: number
  readonly newLines: number
  readonly summary: string
  readonly diagramNodeIds: readonly DiagramNodeId[]
}

export interface PatchChangedFile {
  readonly path: string
  readonly changeType: 'added' | 'modified' | 'deleted' | 'renamed'
  readonly previousPath: string | null
  readonly hunks: readonly PatchHunkRecord[]
}

export interface PatchCommandEvidence {
  readonly evidenceId: string
  readonly command: string
  readonly exitCode: number
  readonly summary: string
  readonly outputSha256: string
}

export interface PatchManifestPayload {
  readonly executionPlanId: ExecutionPlanId
  readonly candidate: StrongFlowCandidateIdentity
  readonly remediationRequestId: RemediationRequestId | null
  readonly changedFiles: readonly PatchChangedFile[]
  readonly commands: readonly PatchCommandEvidence[]
  readonly tests: readonly PatchCommandEvidence[]
}

export interface ReviewFinding {
  readonly findingId: string
  readonly severity: 'blocker' | 'major' | 'minor' | 'note'
  readonly title: string
  readonly message: string
  readonly location: { readonly path: string; readonly hunkId: string } | null
  readonly diagramNodeIds: readonly DiagramNodeId[]
  readonly disposition: 'open' | 'resolved' | 'accepted-risk'
}

export interface ReviewReportPayload {
  readonly patchManifestId: PatchManifestId
  readonly candidate: StrongFlowCandidateIdentity
  readonly outcome: 'accepted' | 'changes-required'
  readonly summary: string
  readonly findings: readonly ReviewFinding[]
}

export interface VerificationCheck {
  readonly checkId: string
  readonly title: string
  readonly command: string | null
  readonly outcome: 'passed' | 'failed' | 'skipped'
  readonly evidence: string
  readonly relatedFindingIds: readonly string[]
}

export interface VerificationReportPayload {
  readonly patchManifestId: PatchManifestId
  readonly candidate: StrongFlowCandidateIdentity
  readonly mode: 'standard' | 'adversarial'
  readonly outcome: 'passed' | 'failed'
  readonly summary: string
  readonly checks: readonly VerificationCheck[]
}

export interface RemediationFindingReference {
  readonly sourceArtifactKind: 'REVIEW_REPORT' | 'VERIFICATION_REPORT'
  readonly sourceArtifactId: string
  readonly findingId: string
  readonly instruction: string
  readonly diagramNodeIds: readonly DiagramNodeId[]
}

export interface RemediationRequestPayload {
  readonly candidate: StrongFlowCandidateIdentity
  readonly patchManifestId: PatchManifestId
  readonly reason: string
  readonly findings: readonly RemediationFindingReference[]
  readonly annotationIds: readonly ExecutionChangeAnnotationId[]
  readonly boundedPaths: readonly string[]
}

export interface RemediationReportPayload {
  readonly remediationRequestId: RemediationRequestId
  readonly patchManifestId: PatchManifestId
  readonly candidate: StrongFlowCandidateIdentity
  readonly summary: string
  readonly addressedFindingIds: readonly string[]
  readonly addressedAnnotationIds: readonly ExecutionChangeAnnotationId[]
  readonly residualRisks: readonly StrongFlowArtifactRisk[]
}

export interface DeliveryReceiptPayload {
  readonly definition: DefinitionIdentity
  readonly humanReviewId: HumanReviewId
  readonly executionPlanId: ExecutionPlanId
  readonly patchManifestId: PatchManifestId
  readonly candidate: StrongFlowCandidateIdentity
  readonly reviewReportId: ReviewReportId
  readonly verificationReportIds: readonly VerificationReportId[]
  readonly remediationReportId: RemediationReportId | null
  readonly summary: string
}

export interface ExecutionChangeAnnotationPayload {
  readonly candidateId: CandidateId
  readonly diffId: GitDiffId
  readonly patchManifestId: PatchManifestId
  readonly diagramId: DiagramId
  readonly diagramKind: 'SYSTEM_ARCHITECTURE_DIAGRAM' | 'PROCESS_FLOW_DIAGRAM'
  readonly nodeId: DiagramNodeId
  readonly location: { readonly path: string; readonly hunkId: string } | null
  readonly message: string
  readonly disposition: 'open' | 'addressed' | 'dismissed'
}

export interface StrongFlowArtifactPayloadByKind {
  readonly USER_REQUEST: UserRequestPayload
  readonly REQUIREMENT_SPEC: RequirementSpecPayload
  readonly SOLUTION_DESIGN: SolutionDesignPayload
  readonly SYSTEM_ARCHITECTURE_DIAGRAM: SystemArchitectureDiagramPayload
  readonly PROCESS_FLOW_DIAGRAM: ProcessFlowDiagramPayload
  readonly HUMAN_REVIEW_RECORD: HumanReviewRecordPayload
  readonly EXECUTION_PLAN: ExecutionPlanPayload
  readonly PATCH_MANIFEST: PatchManifestPayload
  readonly REVIEW_REPORT: ReviewReportPayload
  readonly VERIFICATION_REPORT: VerificationReportPayload
  readonly REMEDIATION_REQUEST: RemediationRequestPayload
  readonly REMEDIATION_REPORT: RemediationReportPayload
  readonly DELIVERY_RECEIPT: DeliveryReceiptPayload
  readonly EXECUTION_CHANGE_ANNOTATION: ExecutionChangeAnnotationPayload
}

export interface StrongFlowArtifactIdByKind {
  readonly USER_REQUEST: UserRequestId
  readonly REQUIREMENT_SPEC: RequirementId
  readonly SOLUTION_DESIGN: SolutionId
  readonly SYSTEM_ARCHITECTURE_DIAGRAM: DiagramId
  readonly PROCESS_FLOW_DIAGRAM: DiagramId
  readonly HUMAN_REVIEW_RECORD: HumanReviewId
  readonly EXECUTION_PLAN: ExecutionPlanId
  readonly PATCH_MANIFEST: PatchManifestId
  readonly REVIEW_REPORT: ReviewReportId
  readonly VERIFICATION_REPORT: VerificationReportId
  readonly REMEDIATION_REQUEST: RemediationRequestId
  readonly REMEDIATION_REPORT: RemediationReportId
  readonly DELIVERY_RECEIPT: DeliveryReceiptId
  readonly EXECUTION_CHANGE_ANNOTATION: ExecutionChangeAnnotationId
}

export interface StrongFlowArtifactMetadata<Kind extends StrongFlowArtifactKind> {
  readonly artifactId: StrongFlowArtifactIdByKind[Kind]
  readonly jobId: JobId
  readonly sourceArtifacts: readonly StrongFlowArtifactReference[]
  readonly producer: StrongFlowArtifactProducer
  readonly kernelEventInterval: StrongFlowArtifactKernelEventInterval | null
  readonly createdAtMillis: number
}

export type StrongFlowArtifactFor<Kind extends StrongFlowArtifactKind> = Readonly<{
  schemaVersion: typeof STRONGFLOW_ARTIFACT_SCHEMA_VERSION
  artifactKind: Kind
  artifactId: StrongFlowArtifactIdByKind[Kind]
  jobId: JobId
  sourceArtifacts: readonly StrongFlowArtifactReference[]
  producer: StrongFlowArtifactProducer
  kernelEventInterval: StrongFlowArtifactKernelEventInterval | null
  createdAtMillis: number
  payload: StrongFlowArtifactPayloadByKind[Kind]
}>

export type StrongFlowArtifact = {
  readonly [Kind in StrongFlowArtifactKind]: StrongFlowArtifactFor<Kind>
}[StrongFlowArtifactKind]

export type UserRequest = StrongFlowArtifactFor<'USER_REQUEST'>
export type RequirementSpec = StrongFlowArtifactFor<'REQUIREMENT_SPEC'>
export type SolutionDesign = StrongFlowArtifactFor<'SOLUTION_DESIGN'>
export type SystemArchitectureDiagram = StrongFlowArtifactFor<'SYSTEM_ARCHITECTURE_DIAGRAM'>
export type ProcessFlowDiagram = StrongFlowArtifactFor<'PROCESS_FLOW_DIAGRAM'>
export type HumanReviewRecord = StrongFlowArtifactFor<'HUMAN_REVIEW_RECORD'>
export type ExecutionPlan = StrongFlowArtifactFor<'EXECUTION_PLAN'>
export type PatchManifest = StrongFlowArtifactFor<'PATCH_MANIFEST'>
export type ReviewReport = StrongFlowArtifactFor<'REVIEW_REPORT'>
export type VerificationReport = StrongFlowArtifactFor<'VERIFICATION_REPORT'>
export type RemediationRequest = StrongFlowArtifactFor<'REMEDIATION_REQUEST'>
export type RemediationReport = StrongFlowArtifactFor<'REMEDIATION_REPORT'>
export type DeliveryReceipt = StrongFlowArtifactFor<'DELIVERY_RECEIPT'>
export type ExecutionChangeAnnotation = StrongFlowArtifactFor<'EXECUTION_CHANGE_ANNOTATION'>

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (!isRecord(value)) {
    artifactError('INVALID_ARTIFACT_SHAPE', path, `${path} must be an object`)
  }
  return value
}

function exactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  path: string,
): void {
  const keys = Object.keys(value)
  if (
    keys.length !== required.length
    || required.some(key => !Object.hasOwn(value, key))
    || keys.some(key => !required.includes(key))
  ) {
    artifactError(
      'INVALID_ARTIFACT_SHAPE',
      path,
      `${path} has an unexpected shape`,
    )
  }
}

function plainText(
  value: unknown,
  path: string,
  options: { readonly allowEmpty?: boolean; readonly maxLength?: number } = {},
): string {
  const maxLength = options.maxLength ?? MAX_TEXT_LENGTH
  if (
    typeof value !== 'string'
    || (!options.allowEmpty && value.length === 0)
    || value.length > maxLength
    || /[\u0000\u000b\u000c\u000e-\u001f\u007f]/u.test(value)
  ) artifactError('INVALID_VALUE', path, `${path} contains invalid text`)
  return value
}

function portableIdentifier(value: unknown, path: string): string {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    artifactError('INVALID_IDENTIFIER', path, `${path} is not a portable identifier`)
  }
  return value
}

function enumValue<const Values extends readonly string[]>(
  value: unknown,
  allowed: Values,
  path: string,
): Values[number] {
  if (typeof value !== 'string' || !allowed.includes(value)) {
    artifactError('INVALID_VALUE', path, `${path} is unsupported`)
  }
  return value as Values[number]
}

function nonNegativeInteger(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0) {
    artifactError('INVALID_VALUE', path, `${path} must be a non-negative safe integer`)
  }
  return Number(value)
}

function positiveInteger(value: unknown, path: string): number {
  const result = nonNegativeInteger(value, path)
  if (result === 0) artifactError('INVALID_VALUE', path, `${path} must be positive`)
  return result
}

function nullableText(value: unknown, path: string): string | null {
  return value === null ? null : plainText(value, path)
}

function arrayOf<Value>(
  value: unknown,
  path: string,
  parser: (entry: unknown, entryPath: string) => Value,
  options: { readonly minLength?: number; readonly maxLength?: number } = {},
): readonly Value[] {
  if (!Array.isArray(value)) artifactError('INVALID_VALUE', path, `${path} must be an array`)
  const minLength = options.minLength ?? 0
  const maxLength = options.maxLength ?? 10_000
  if (value.length < minLength || value.length > maxLength) {
    artifactError('INVALID_VALUE', path, `${path} has an invalid number of entries`)
  }
  return Object.freeze(value.map((entry, index) => parser(entry, `${path}[${index}]`)))
}

function uniqueIdentifiers(values: readonly string[], path: string): void {
  if (new Set(values).size !== values.length) {
    artifactError('INVALID_RELATIONSHIP', path, `${path} contains repeated identifiers`)
  }
}

function identifierArray(value: unknown, path: string): readonly string[] {
  const result = arrayOf(value, path, portableIdentifier)
  uniqueIdentifiers(result, path)
  return result
}

function relativePath(value: unknown, path: string): string {
  const result = plainText(value, path, { maxLength: 4_096 })
  const parts = result.split('/')
  if (
    result.startsWith('/')
    || result.includes('\\')
    || parts.some(part => part.length === 0 || part === '.' || part === '..')
  ) artifactError('INVALID_VALUE', path, `${path} must be a normalized relative path`)
  return result
}

function pathArray(value: unknown, path: string): readonly string[] {
  const result = arrayOf(value, path, relativePath)
  uniqueIdentifiers(result, path)
  return result
}

function parseStatement(value: unknown, path: string): StrongFlowArtifactStatement {
  const input = record(value, path)
  exactKeys(input, ['id', 'text'], path)
  return Object.freeze({
    id: portableIdentifier(input.id, `${path}.id`),
    text: plainText(input.text, `${path}.text`),
  })
}

function parseStatements(
  value: unknown,
  path: string,
  minLength = 0,
): readonly StrongFlowArtifactStatement[] {
  const result = arrayOf(value, path, parseStatement, { minLength })
  uniqueIdentifiers(result.map(entry => entry.id), path)
  return result
}

function parseRisk(value: unknown, path: string): StrongFlowArtifactRisk {
  const input = record(value, path)
  exactKeys(input, ['riskId', 'statement', 'mitigation'], path)
  return Object.freeze({
    riskId: portableIdentifier(input.riskId, `${path}.riskId`),
    statement: plainText(input.statement, `${path}.statement`),
    mitigation: nullableText(input.mitigation, `${path}.mitigation`),
  })
}

function parseRisks(value: unknown, path: string): readonly StrongFlowArtifactRisk[] {
  const result = arrayOf(value, path, parseRisk)
  uniqueIdentifiers(result.map(entry => entry.riskId), path)
  return result
}

function parseDefinitionIdentity(value: unknown, path: string): DefinitionIdentity {
  const input = record(value, path)
  exactKeys(input, [
    'requirementId',
    'solutionId',
    'systemArchitectureDiagramId',
    'processFlowDiagramId',
  ], path)
  return Object.freeze({
    requirementId: portableIdentifier(
      input.requirementId,
      `${path}.requirementId`,
    ) as RequirementId,
    solutionId: portableIdentifier(input.solutionId, `${path}.solutionId`) as SolutionId,
    systemArchitectureDiagramId: portableIdentifier(
      input.systemArchitectureDiagramId,
      `${path}.systemArchitectureDiagramId`,
    ) as DiagramId,
    processFlowDiagramId: portableIdentifier(
      input.processFlowDiagramId,
      `${path}.processFlowDiagramId`,
    ) as DiagramId,
  })
}

export function parseStrongFlowCandidateIdentity(
  value: unknown,
  path: string,
): StrongFlowCandidateIdentity {
  const input = record(value, path)
  exactKeys(input, [
    'candidateId',
    'sourceSnapshotId',
    'baseCommitId',
    'baseTreeId',
    'candidateCommitId',
    'candidateTreeId',
    'diffId',
  ], path)
  const gitObject = (entry: unknown, entryPath: string): string => {
    if (typeof entry !== 'string' || !GIT_OBJECT_PATTERN.test(entry)) {
      artifactError('INVALID_IDENTIFIER', entryPath, `${entryPath} is not a Git object id`)
    }
    return entry
  }
  if (typeof input.sourceSnapshotId !== 'string'
    || !SOURCE_SNAPSHOT_PATTERN.test(input.sourceSnapshotId)) {
    artifactError(
      'INVALID_IDENTIFIER',
      `${path}.sourceSnapshotId`,
      `${path}.sourceSnapshotId is invalid`,
    )
  }
  if (typeof input.diffId !== 'string' || !SHA256_PATTERN.test(input.diffId)) {
    artifactError('INVALID_IDENTIFIER', `${path}.diffId`, `${path}.diffId is invalid`)
  }
  return Object.freeze({
    candidateId: portableIdentifier(input.candidateId, `${path}.candidateId`) as CandidateId,
    sourceSnapshotId: input.sourceSnapshotId as SourceSnapshotId,
    baseCommitId: gitObject(input.baseCommitId, `${path}.baseCommitId`) as GitCommitId,
    baseTreeId: gitObject(input.baseTreeId, `${path}.baseTreeId`) as GitTreeId,
    candidateCommitId: gitObject(
      input.candidateCommitId,
      `${path}.candidateCommitId`,
    ) as GitCommitId,
    candidateTreeId: gitObject(input.candidateTreeId, `${path}.candidateTreeId`) as GitTreeId,
    diffId: input.diffId as GitDiffId,
  })
}

function parseUserRequest(value: unknown, path: string): UserRequestPayload {
  const input = record(value, path)
  exactKeys(input, ['request', 'submittedFrom'], path)
  return Object.freeze({
    request: plainText(input.request, `${path}.request`),
    submittedFrom: enumValue(
      input.submittedFrom,
      ['chat', 'strongflow-workbench', 'cli'] as const,
      `${path}.submittedFrom`,
    ),
  })
}

function parseRequirementSpec(value: unknown, path: string): RequirementSpecPayload {
  const input = record(value, path)
  exactKeys(input, [
    'title',
    'summary',
    'goals',
    'nonGoals',
    'constraints',
    'acceptanceCriteria',
    'repositoryFacts',
    'risks',
    'openQuestions',
  ], path)
  const acceptanceCriteria = arrayOf(
    input.acceptanceCriteria,
    `${path}.acceptanceCriteria`,
    (entry, entryPath): RequirementAcceptanceCriterion => {
      const criterion = record(entry, entryPath)
      exactKeys(criterion, ['criterionId', 'statement', 'verification'], entryPath)
      return Object.freeze({
        criterionId: portableIdentifier(criterion.criterionId, `${entryPath}.criterionId`),
        statement: plainText(criterion.statement, `${entryPath}.statement`),
        verification: plainText(criterion.verification, `${entryPath}.verification`),
      })
    },
    { minLength: 1 },
  )
  uniqueIdentifiers(
    acceptanceCriteria.map(entry => entry.criterionId),
    `${path}.acceptanceCriteria`,
  )
  const repositoryFacts = arrayOf(
    input.repositoryFacts,
    `${path}.repositoryFacts`,
    (entry, entryPath): RequirementRepositoryFact => {
      const fact = record(entry, entryPath)
      exactKeys(fact, ['factId', 'statement', 'evidence'], entryPath)
      return Object.freeze({
        factId: portableIdentifier(fact.factId, `${entryPath}.factId`),
        statement: plainText(fact.statement, `${entryPath}.statement`),
        evidence: plainText(fact.evidence, `${entryPath}.evidence`),
      })
    },
  )
  uniqueIdentifiers(repositoryFacts.map(entry => entry.factId), `${path}.repositoryFacts`)
  const openQuestions = arrayOf(
    input.openQuestions,
    `${path}.openQuestions`,
    (entry, entryPath): RequirementOpenQuestion => {
      const question = record(entry, entryPath)
      exactKeys(question, ['questionId', 'question', 'blocking'], entryPath)
      if (typeof question.blocking !== 'boolean') {
        artifactError('INVALID_VALUE', `${entryPath}.blocking`, 'blocking must be boolean')
      }
      return Object.freeze({
        questionId: portableIdentifier(question.questionId, `${entryPath}.questionId`),
        question: plainText(question.question, `${entryPath}.question`),
        blocking: question.blocking,
      })
    },
  )
  uniqueIdentifiers(openQuestions.map(entry => entry.questionId), `${path}.openQuestions`)
  return Object.freeze({
    title: plainText(input.title, `${path}.title`),
    summary: plainText(input.summary, `${path}.summary`),
    goals: parseStatements(input.goals, `${path}.goals`, 1),
    nonGoals: parseStatements(input.nonGoals, `${path}.nonGoals`),
    constraints: parseStatements(input.constraints, `${path}.constraints`),
    acceptanceCriteria,
    repositoryFacts,
    risks: parseRisks(input.risks, `${path}.risks`),
    openQuestions,
  })
}

function parseSolutionDesign(value: unknown, path: string): SolutionDesignPayload {
  const input = record(value, path)
  exactKeys(input, [
    'requirementId',
    'summary',
    'decisions',
    'components',
    'connections',
    'unresolvedFacts',
    'risks',
  ], path)
  const decisions = arrayOf(
    input.decisions,
    `${path}.decisions`,
    (entry, entryPath): SolutionDecision => {
      const decision = record(entry, entryPath)
      exactKeys(decision, [
        'decisionId',
        'title',
        'decision',
        'rationale',
        'requirementItemIds',
      ], entryPath)
      return Object.freeze({
        decisionId: portableIdentifier(decision.decisionId, `${entryPath}.decisionId`),
        title: plainText(decision.title, `${entryPath}.title`),
        decision: plainText(decision.decision, `${entryPath}.decision`),
        rationale: plainText(decision.rationale, `${entryPath}.rationale`),
        requirementItemIds: identifierArray(
          decision.requirementItemIds,
          `${entryPath}.requirementItemIds`,
        ),
      })
    },
    { minLength: 1 },
  )
  uniqueIdentifiers(decisions.map(entry => entry.decisionId), `${path}.decisions`)
  const components = arrayOf(
    input.components,
    `${path}.components`,
    (entry, entryPath): SolutionComponent => {
      const component = record(entry, entryPath)
      exactKeys(component, [
        'componentId',
        'name',
        'kind',
        'responsibility',
        'trustBoundary',
        'sourcePaths',
      ], entryPath)
      return Object.freeze({
        componentId: portableIdentifier(component.componentId, `${entryPath}.componentId`),
        name: plainText(component.name, `${entryPath}.name`),
        kind: enumValue(
          component.kind,
          ['surface', 'service', 'module', 'store', 'external'] as const,
          `${entryPath}.kind`,
        ),
        responsibility: plainText(component.responsibility, `${entryPath}.responsibility`),
        trustBoundary: nullableText(
          component.trustBoundary,
          `${entryPath}.trustBoundary`,
        ),
        sourcePaths: pathArray(component.sourcePaths, `${entryPath}.sourcePaths`),
      })
    },
    { minLength: 1 },
  )
  const componentIds = components.map(entry => entry.componentId)
  uniqueIdentifiers(componentIds, `${path}.components`)
  const connections = arrayOf(
    input.connections,
    `${path}.connections`,
    (entry, entryPath): SolutionConnection => {
      const connection = record(entry, entryPath)
      exactKeys(connection, [
        'connectionId',
        'fromComponentId',
        'toComponentId',
        'label',
      ], entryPath)
      const result = Object.freeze({
        connectionId: portableIdentifier(connection.connectionId, `${entryPath}.connectionId`),
        fromComponentId: portableIdentifier(
          connection.fromComponentId,
          `${entryPath}.fromComponentId`,
        ),
        toComponentId: portableIdentifier(
          connection.toComponentId,
          `${entryPath}.toComponentId`,
        ),
        label: plainText(connection.label, `${entryPath}.label`),
      })
      if (!componentIds.includes(result.fromComponentId)
        || !componentIds.includes(result.toComponentId)) {
        artifactError(
          'INVALID_RELATIONSHIP',
          entryPath,
          `${entryPath} references an unknown component`,
        )
      }
      return result
    },
  )
  uniqueIdentifiers(connections.map(entry => entry.connectionId), `${path}.connections`)
  const unresolvedFacts = arrayOf(
    input.unresolvedFacts,
    `${path}.unresolvedFacts`,
    (entry, entryPath): SolutionUnresolvedFact => {
      const fact = record(entry, entryPath)
      exactKeys(fact, ['factId', 'question', 'impact'], entryPath)
      return Object.freeze({
        factId: portableIdentifier(fact.factId, `${entryPath}.factId`),
        question: plainText(fact.question, `${entryPath}.question`),
        impact: plainText(fact.impact, `${entryPath}.impact`),
      })
    },
  )
  uniqueIdentifiers(unresolvedFacts.map(entry => entry.factId), `${path}.unresolvedFacts`)
  return Object.freeze({
    requirementId: portableIdentifier(
      input.requirementId,
      `${path}.requirementId`,
    ) as RequirementId,
    summary: plainText(input.summary, `${path}.summary`),
    decisions,
    components,
    connections,
    unresolvedFacts,
    risks: parseRisks(input.risks, `${path}.risks`),
  })
}

function parseDiagramEdge(value: unknown, path: string): StrongFlowDiagramEdge {
  const input = record(value, path)
  exactKeys(input, ['edgeId', 'fromNodeId', 'toNodeId', 'label'], path)
  return Object.freeze({
    edgeId: portableIdentifier(input.edgeId, `${path}.edgeId`),
    fromNodeId: DiagramNodeId(portableIdentifier(input.fromNodeId, `${path}.fromNodeId`)),
    toNodeId: DiagramNodeId(portableIdentifier(input.toNodeId, `${path}.toNodeId`)),
    label: plainText(input.label, `${path}.label`, { allowEmpty: true }),
  })
}

function validateDiagramEdges(
  edges: readonly StrongFlowDiagramEdge[],
  nodeIds: readonly DiagramNodeId[],
  path: string,
): void {
  uniqueIdentifiers(edges.map(entry => entry.edgeId), path)
  for (const edge of edges) {
    if (!nodeIds.includes(edge.fromNodeId) || !nodeIds.includes(edge.toNodeId)) {
      artifactError('INVALID_RELATIONSHIP', path, `${path} references an unknown node`)
    }
  }
}

function diagramBase(
  input: Record<string, unknown>,
  path: string,
): Omit<StrongFlowDiagramPayloadBase, 'edges'> {
  return Object.freeze({
    requirementId: portableIdentifier(
      input.requirementId,
      `${path}.requirementId`,
    ) as RequirementId,
    solutionId: portableIdentifier(input.solutionId, `${path}.solutionId`) as SolutionId,
    title: plainText(input.title, `${path}.title`),
  })
}

function parseSystemArchitectureDiagram(
  value: unknown,
  path: string,
): SystemArchitectureDiagramPayload {
  const input = record(value, path)
  exactKeys(input, ['requirementId', 'solutionId', 'title', 'nodes', 'edges'], path)
  const nodes = arrayOf(
    input.nodes,
    `${path}.nodes`,
    (entry, entryPath): SystemArchitectureDiagramNode => {
      const node = record(entry, entryPath)
      exactKeys(node, [
        'nodeId',
        'label',
        'kind',
        'description',
        'trustBoundary',
        'unresolved',
        'componentIds',
        'sourcePaths',
      ], entryPath)
      if (typeof node.unresolved !== 'boolean') {
        artifactError('INVALID_VALUE', `${entryPath}.unresolved`, 'unresolved must be boolean')
      }
      return Object.freeze({
        nodeId: DiagramNodeId(portableIdentifier(node.nodeId, `${entryPath}.nodeId`)),
        label: plainText(node.label, `${entryPath}.label`),
        kind: enumValue(
          node.kind,
          [
            'actor',
            'surface',
            'service',
            'module',
            'store',
            'external',
            'boundary',
            'unresolved',
          ] as const,
          `${entryPath}.kind`,
        ),
        description: plainText(node.description, `${entryPath}.description`),
        trustBoundary: nullableText(node.trustBoundary, `${entryPath}.trustBoundary`),
        unresolved: node.unresolved,
        componentIds: identifierArray(node.componentIds, `${entryPath}.componentIds`),
        sourcePaths: pathArray(node.sourcePaths, `${entryPath}.sourcePaths`),
      })
    },
    { minLength: 1 },
  )
  const nodeIds = nodes.map(entry => entry.nodeId)
  uniqueIdentifiers(nodeIds, `${path}.nodes`)
  const edges = arrayOf(input.edges, `${path}.edges`, parseDiagramEdge)
  validateDiagramEdges(edges, nodeIds, `${path}.edges`)
  return Object.freeze({ ...diagramBase(input, path), nodes, edges })
}

function parseProcessFlowDiagram(value: unknown, path: string): ProcessFlowDiagramPayload {
  const input = record(value, path)
  exactKeys(input, ['requirementId', 'solutionId', 'title', 'nodes', 'edges'], path)
  const nodes = arrayOf(
    input.nodes,
    `${path}.nodes`,
    (entry, entryPath): ProcessFlowDiagramNode => {
      const node = record(entry, entryPath)
      exactKeys(
        node,
        ['nodeId', 'label', 'kind', 'description', 'roleId', 'unresolved'],
        entryPath,
      )
      if (typeof node.unresolved !== 'boolean') {
        artifactError('INVALID_VALUE', `${entryPath}.unresolved`, 'unresolved must be boolean')
      }
      const roleId = node.roleId === null
        ? null
        : enumValue(node.roleId, STRONGFLOW_ROLE_IDS, `${entryPath}.roleId`)
      return Object.freeze({
        nodeId: DiagramNodeId(portableIdentifier(node.nodeId, `${entryPath}.nodeId`)),
        label: plainText(node.label, `${entryPath}.label`),
        kind: enumValue(
          node.kind,
          ['start', 'stage', 'human-review', 'decision', 'state', 'end'] as const,
          `${entryPath}.kind`,
        ),
        description: plainText(node.description, `${entryPath}.description`),
        roleId,
        unresolved: node.unresolved,
      })
    },
    { minLength: 1 },
  )
  const nodeIds = nodes.map(entry => entry.nodeId)
  uniqueIdentifiers(nodeIds, `${path}.nodes`)
  const edges = arrayOf(input.edges, `${path}.edges`, parseDiagramEdge)
  validateDiagramEdges(edges, nodeIds, `${path}.edges`)
  return Object.freeze({ ...diagramBase(input, path), nodes, edges })
}

function parseHumanReviewRecord(value: unknown, path: string): HumanReviewRecordPayload {
  const input = record(value, path)
  exactKeys(input, ['definition', 'decision', 'comment', 'scope'], path)
  const decision = enumValue(
    input.decision,
    ['approved', 'changes-requested', 'rejected'] as const,
    `${path}.decision`,
  )
  const scope = input.scope === null
    ? null
    : enumValue(
      input.scope,
      ['requirements', 'solution', 'diagrams'] as const,
      `${path}.scope`,
    )
  if ((decision === 'changes-requested') !== (scope !== null)) {
    artifactError(
      'INVALID_RELATIONSHIP',
      `${path}.scope`,
      'only a changes-requested review may carry a revision scope',
    )
  }
  return Object.freeze({
    definition: parseDefinitionIdentity(input.definition, `${path}.definition`),
    decision,
    comment: nullableText(input.comment, `${path}.comment`),
    scope,
  })
}

function parseExecutionPlan(value: unknown, path: string): ExecutionPlanPayload {
  const input = record(value, path)
  exactKeys(input, ['definition', 'humanReviewId', 'summary', 'steps'], path)
  const steps = arrayOf(
    input.steps,
    `${path}.steps`,
    (entry, entryPath): ExecutionPlanStep => {
      const step = record(entry, entryPath)
      exactKeys(step, [
        'stepId',
        'title',
        'instructions',
        'dependsOn',
        'paths',
        'commands',
        'checks',
      ], entryPath)
      return Object.freeze({
        stepId: portableIdentifier(step.stepId, `${entryPath}.stepId`),
        title: plainText(step.title, `${entryPath}.title`),
        instructions: plainText(step.instructions, `${entryPath}.instructions`),
        dependsOn: identifierArray(step.dependsOn, `${entryPath}.dependsOn`),
        paths: pathArray(step.paths, `${entryPath}.paths`),
        commands: arrayOf(step.commands, `${entryPath}.commands`, plainText),
        checks: arrayOf(step.checks, `${entryPath}.checks`, plainText, { minLength: 1 }),
      })
    },
    { minLength: 1 },
  )
  const stepIds = steps.map(entry => entry.stepId)
  uniqueIdentifiers(stepIds, `${path}.steps`)
  for (const [index, step] of steps.entries()) {
    const prior = new Set(stepIds.slice(0, index))
    if (step.dependsOn.some(dependency => !prior.has(dependency))) {
      artifactError(
        'INVALID_RELATIONSHIP',
        `${path}.steps[${index}].dependsOn`,
        'plan dependencies must reference an earlier step',
      )
    }
  }
  return Object.freeze({
    definition: parseDefinitionIdentity(input.definition, `${path}.definition`),
    humanReviewId: portableIdentifier(
      input.humanReviewId,
      `${path}.humanReviewId`,
    ) as HumanReviewId,
    summary: plainText(input.summary, `${path}.summary`),
    steps,
  })
}

function parsePatchHunk(value: unknown, path: string): PatchHunkRecord {
  const input = record(value, path)
  exactKeys(input, [
    'hunkId',
    'oldStart',
    'oldLines',
    'newStart',
    'newLines',
    'summary',
    'diagramNodeIds',
  ], path)
  return Object.freeze({
    hunkId: portableIdentifier(input.hunkId, `${path}.hunkId`),
    oldStart: nonNegativeInteger(input.oldStart, `${path}.oldStart`),
    oldLines: nonNegativeInteger(input.oldLines, `${path}.oldLines`),
    newStart: nonNegativeInteger(input.newStart, `${path}.newStart`),
    newLines: nonNegativeInteger(input.newLines, `${path}.newLines`),
    summary: plainText(input.summary, `${path}.summary`),
    diagramNodeIds: identifierArray(
      input.diagramNodeIds,
      `${path}.diagramNodeIds`,
    ) as readonly DiagramNodeId[],
  })
}

function parseChangedFile(value: unknown, path: string): PatchChangedFile {
  const input = record(value, path)
  exactKeys(input, ['path', 'changeType', 'previousPath', 'hunks'], path)
  const changeType = enumValue(
    input.changeType,
    ['added', 'modified', 'deleted', 'renamed'] as const,
    `${path}.changeType`,
  )
  const previousPath = input.previousPath === null
    ? null
    : relativePath(input.previousPath, `${path}.previousPath`)
  if ((changeType === 'renamed') !== (previousPath !== null)) {
    artifactError(
      'INVALID_RELATIONSHIP',
      `${path}.previousPath`,
      'only a renamed file must carry a previous path',
    )
  }
  const hunks = arrayOf(input.hunks, `${path}.hunks`, parsePatchHunk)
  uniqueIdentifiers(hunks.map(entry => entry.hunkId), `${path}.hunks`)
  return Object.freeze({
    path: relativePath(input.path, `${path}.path`),
    changeType,
    previousPath,
    hunks,
  })
}

function parseCommandEvidence(value: unknown, path: string): PatchCommandEvidence {
  const input = record(value, path)
  exactKeys(input, ['evidenceId', 'command', 'exitCode', 'summary', 'outputSha256'], path)
  if (!Number.isSafeInteger(input.exitCode)) {
    artifactError('INVALID_VALUE', `${path}.exitCode`, 'exitCode must be a safe integer')
  }
  if (typeof input.outputSha256 !== 'string' || !SHA256_PATTERN.test(input.outputSha256)) {
    artifactError('INVALID_IDENTIFIER', `${path}.outputSha256`, 'outputSha256 is invalid')
  }
  return Object.freeze({
    evidenceId: portableIdentifier(input.evidenceId, `${path}.evidenceId`),
    command: plainText(input.command, `${path}.command`),
    exitCode: Number(input.exitCode),
    summary: plainText(input.summary, `${path}.summary`),
    outputSha256: input.outputSha256,
  })
}

function parsePatchManifest(value: unknown, path: string): PatchManifestPayload {
  const input = record(value, path)
  exactKeys(input, [
    'executionPlanId',
    'candidate',
    'remediationRequestId',
    'changedFiles',
    'commands',
    'tests',
  ], path)
  const changedFiles = arrayOf(input.changedFiles, `${path}.changedFiles`, parseChangedFile)
  uniqueIdentifiers(changedFiles.map(entry => entry.path), `${path}.changedFiles`)
  const commands = arrayOf(input.commands, `${path}.commands`, parseCommandEvidence)
  const tests = arrayOf(input.tests, `${path}.tests`, parseCommandEvidence)
  uniqueIdentifiers(
    [...commands, ...tests].map(entry => entry.evidenceId),
    `${path}.commands`,
  )
  return Object.freeze({
    executionPlanId: ExecutionPlanId(
      portableIdentifier(input.executionPlanId, `${path}.executionPlanId`),
    ),
    candidate: parseStrongFlowCandidateIdentity(input.candidate, `${path}.candidate`),
    remediationRequestId: input.remediationRequestId === null
      ? null
      : RemediationRequestId(
        portableIdentifier(input.remediationRequestId, `${path}.remediationRequestId`),
      ),
    changedFiles,
    commands,
    tests,
  })
}

function parseFindingLocation(
  value: unknown,
  path: string,
): ReviewFinding['location'] {
  if (value === null) return null
  const input = record(value, path)
  exactKeys(input, ['path', 'hunkId'], path)
  return Object.freeze({
    path: relativePath(input.path, `${path}.path`),
    hunkId: portableIdentifier(input.hunkId, `${path}.hunkId`),
  })
}

function parseReviewFinding(value: unknown, path: string): ReviewFinding {
  const input = record(value, path)
  exactKeys(input, [
    'findingId',
    'severity',
    'title',
    'message',
    'location',
    'diagramNodeIds',
    'disposition',
  ], path)
  return Object.freeze({
    findingId: portableIdentifier(input.findingId, `${path}.findingId`),
    severity: enumValue(
      input.severity,
      ['blocker', 'major', 'minor', 'note'] as const,
      `${path}.severity`,
    ),
    title: plainText(input.title, `${path}.title`),
    message: plainText(input.message, `${path}.message`),
    location: parseFindingLocation(input.location, `${path}.location`),
    diagramNodeIds: identifierArray(
      input.diagramNodeIds,
      `${path}.diagramNodeIds`,
    ) as readonly DiagramNodeId[],
    disposition: enumValue(
      input.disposition,
      ['open', 'resolved', 'accepted-risk'] as const,
      `${path}.disposition`,
    ),
  })
}

function parseReviewReport(value: unknown, path: string): ReviewReportPayload {
  const input = record(value, path)
  exactKeys(input, ['patchManifestId', 'candidate', 'outcome', 'summary', 'findings'], path)
  const findings = arrayOf(input.findings, `${path}.findings`, parseReviewFinding)
  uniqueIdentifiers(findings.map(entry => entry.findingId), `${path}.findings`)
  return Object.freeze({
    patchManifestId: PatchManifestId(
      portableIdentifier(input.patchManifestId, `${path}.patchManifestId`),
    ),
    candidate: parseStrongFlowCandidateIdentity(input.candidate, `${path}.candidate`),
    outcome: enumValue(
      input.outcome,
      ['accepted', 'changes-required'] as const,
      `${path}.outcome`,
    ),
    summary: plainText(input.summary, `${path}.summary`),
    findings,
  })
}

function parseVerificationCheck(value: unknown, path: string): VerificationCheck {
  const input = record(value, path)
  exactKeys(input, [
    'checkId',
    'title',
    'command',
    'outcome',
    'evidence',
    'relatedFindingIds',
  ], path)
  return Object.freeze({
    checkId: portableIdentifier(input.checkId, `${path}.checkId`),
    title: plainText(input.title, `${path}.title`),
    command: nullableText(input.command, `${path}.command`),
    outcome: enumValue(
      input.outcome,
      ['passed', 'failed', 'skipped'] as const,
      `${path}.outcome`,
    ),
    evidence: plainText(input.evidence, `${path}.evidence`),
    relatedFindingIds: identifierArray(
      input.relatedFindingIds,
      `${path}.relatedFindingIds`,
    ),
  })
}

function parseVerificationReport(value: unknown, path: string): VerificationReportPayload {
  const input = record(value, path)
  exactKeys(input, [
    'patchManifestId',
    'candidate',
    'mode',
    'outcome',
    'summary',
    'checks',
  ], path)
  const checks = arrayOf(input.checks, `${path}.checks`, parseVerificationCheck, {
    minLength: 1,
  })
  uniqueIdentifiers(checks.map(entry => entry.checkId), `${path}.checks`)
  return Object.freeze({
    patchManifestId: PatchManifestId(
      portableIdentifier(input.patchManifestId, `${path}.patchManifestId`),
    ),
    candidate: parseStrongFlowCandidateIdentity(input.candidate, `${path}.candidate`),
    mode: enumValue(input.mode, ['standard', 'adversarial'] as const, `${path}.mode`),
    outcome: enumValue(input.outcome, ['passed', 'failed'] as const, `${path}.outcome`),
    summary: plainText(input.summary, `${path}.summary`),
    checks,
  })
}

function parseRemediationFinding(
  value: unknown,
  path: string,
): RemediationFindingReference {
  const input = record(value, path)
  exactKeys(input, [
    'sourceArtifactKind',
    'sourceArtifactId',
    'findingId',
    'instruction',
    'diagramNodeIds',
  ], path)
  return Object.freeze({
    sourceArtifactKind: enumValue(
      input.sourceArtifactKind,
      ['REVIEW_REPORT', 'VERIFICATION_REPORT'] as const,
      `${path}.sourceArtifactKind`,
    ),
    sourceArtifactId: portableIdentifier(
      input.sourceArtifactId,
      `${path}.sourceArtifactId`,
    ),
    findingId: portableIdentifier(input.findingId, `${path}.findingId`),
    instruction: plainText(input.instruction, `${path}.instruction`),
    diagramNodeIds: identifierArray(
      input.diagramNodeIds,
      `${path}.diagramNodeIds`,
    ) as readonly DiagramNodeId[],
  })
}

function parseAnnotationIds(
  value: unknown,
  path: string,
): readonly ExecutionChangeAnnotationId[] {
  return identifierArray(value, path).map(ExecutionChangeAnnotationId)
}

function parseRemediationRequest(value: unknown, path: string): RemediationRequestPayload {
  const input = record(value, path)
  exactKeys(input, [
    'candidate',
    'patchManifestId',
    'reason',
    'findings',
    'annotationIds',
    'boundedPaths',
  ], path)
  const findings = arrayOf(input.findings, `${path}.findings`, parseRemediationFinding)
  uniqueIdentifiers(
    findings.map(entry => `${entry.sourceArtifactId}:${entry.findingId}`),
    `${path}.findings`,
  )
  return Object.freeze({
    candidate: parseStrongFlowCandidateIdentity(input.candidate, `${path}.candidate`),
    patchManifestId: PatchManifestId(
      portableIdentifier(input.patchManifestId, `${path}.patchManifestId`),
    ),
    reason: plainText(input.reason, `${path}.reason`),
    findings,
    annotationIds: Object.freeze(parseAnnotationIds(input.annotationIds, `${path}.annotationIds`)),
    boundedPaths: pathArray(input.boundedPaths, `${path}.boundedPaths`),
  })
}

function parseRemediationReport(value: unknown, path: string): RemediationReportPayload {
  const input = record(value, path)
  exactKeys(input, [
    'remediationRequestId',
    'patchManifestId',
    'candidate',
    'summary',
    'addressedFindingIds',
    'addressedAnnotationIds',
    'residualRisks',
  ], path)
  return Object.freeze({
    remediationRequestId: RemediationRequestId(
      portableIdentifier(input.remediationRequestId, `${path}.remediationRequestId`),
    ),
    patchManifestId: PatchManifestId(
      portableIdentifier(input.patchManifestId, `${path}.patchManifestId`),
    ),
    candidate: parseStrongFlowCandidateIdentity(input.candidate, `${path}.candidate`),
    summary: plainText(input.summary, `${path}.summary`),
    addressedFindingIds: identifierArray(
      input.addressedFindingIds,
      `${path}.addressedFindingIds`,
    ),
    addressedAnnotationIds: Object.freeze(parseAnnotationIds(
      input.addressedAnnotationIds,
      `${path}.addressedAnnotationIds`,
    )),
    residualRisks: parseRisks(input.residualRisks, `${path}.residualRisks`),
  })
}

function parseVerificationReportIds(
  value: unknown,
  path: string,
): readonly VerificationReportId[] {
  const result = identifierArray(value, path).map(VerificationReportId)
  if (result.length === 0) {
    artifactError('INVALID_VALUE', path, `${path} must contain at least one report`)
  }
  return Object.freeze(result)
}

function parseDeliveryReceipt(value: unknown, path: string): DeliveryReceiptPayload {
  const input = record(value, path)
  exactKeys(input, [
    'definition',
    'humanReviewId',
    'executionPlanId',
    'patchManifestId',
    'candidate',
    'reviewReportId',
    'verificationReportIds',
    'remediationReportId',
    'summary',
  ], path)
  return Object.freeze({
    definition: parseDefinitionIdentity(input.definition, `${path}.definition`),
    humanReviewId: portableIdentifier(
      input.humanReviewId,
      `${path}.humanReviewId`,
    ) as HumanReviewId,
    executionPlanId: ExecutionPlanId(
      portableIdentifier(input.executionPlanId, `${path}.executionPlanId`),
    ),
    patchManifestId: PatchManifestId(
      portableIdentifier(input.patchManifestId, `${path}.patchManifestId`),
    ),
    candidate: parseStrongFlowCandidateIdentity(input.candidate, `${path}.candidate`),
    reviewReportId: ReviewReportId(
      portableIdentifier(input.reviewReportId, `${path}.reviewReportId`),
    ),
    verificationReportIds: parseVerificationReportIds(
      input.verificationReportIds,
      `${path}.verificationReportIds`,
    ),
    remediationReportId: input.remediationReportId === null
      ? null
      : RemediationReportId(
        portableIdentifier(input.remediationReportId, `${path}.remediationReportId`),
      ),
    summary: plainText(input.summary, `${path}.summary`),
  })
}

function parseExecutionChangeAnnotation(
  value: unknown,
  path: string,
): ExecutionChangeAnnotationPayload {
  const input = record(value, path)
  exactKeys(input, [
    'candidateId',
    'diffId',
    'patchManifestId',
    'diagramId',
    'diagramKind',
    'nodeId',
    'location',
    'message',
    'disposition',
  ], path)
  if (typeof input.diffId !== 'string' || !SHA256_PATTERN.test(input.diffId)) {
    artifactError('INVALID_IDENTIFIER', `${path}.diffId`, `${path}.diffId is invalid`)
  }
  return Object.freeze({
    candidateId: portableIdentifier(input.candidateId, `${path}.candidateId`) as CandidateId,
    diffId: input.diffId as GitDiffId,
    patchManifestId: PatchManifestId(
      portableIdentifier(input.patchManifestId, `${path}.patchManifestId`),
    ),
    diagramId: portableIdentifier(input.diagramId, `${path}.diagramId`) as DiagramId,
    diagramKind: enumValue(
      input.diagramKind,
      ['SYSTEM_ARCHITECTURE_DIAGRAM', 'PROCESS_FLOW_DIAGRAM'] as const,
      `${path}.diagramKind`,
    ),
    nodeId: DiagramNodeId(portableIdentifier(input.nodeId, `${path}.nodeId`)),
    location: parseFindingLocation(input.location, `${path}.location`),
    message: plainText(input.message, `${path}.message`),
    disposition: enumValue(
      input.disposition,
      ['open', 'addressed', 'dismissed'] as const,
      `${path}.disposition`,
    ),
  })
}

/** Validates a model-owned payload before trusted identity and provenance are added. */
export function parseStrongFlowArtifactPayload<Kind extends StrongFlowArtifactKind>(
  kind: Kind,
  value: unknown,
): StrongFlowArtifactPayloadByKind[Kind] {
  const path = `${kind}.payload`
  let result: StrongFlowArtifactPayloadByKind[StrongFlowArtifactKind]
  switch (kind) {
    case 'USER_REQUEST': result = parseUserRequest(value, path); break
    case 'REQUIREMENT_SPEC': result = parseRequirementSpec(value, path); break
    case 'SOLUTION_DESIGN': result = parseSolutionDesign(value, path); break
    case 'SYSTEM_ARCHITECTURE_DIAGRAM':
      result = parseSystemArchitectureDiagram(value, path)
      break
    case 'PROCESS_FLOW_DIAGRAM': result = parseProcessFlowDiagram(value, path); break
    case 'HUMAN_REVIEW_RECORD': result = parseHumanReviewRecord(value, path); break
    case 'EXECUTION_PLAN': result = parseExecutionPlan(value, path); break
    case 'PATCH_MANIFEST': result = parsePatchManifest(value, path); break
    case 'REVIEW_REPORT': result = parseReviewReport(value, path); break
    case 'VERIFICATION_REPORT': result = parseVerificationReport(value, path); break
    case 'REMEDIATION_REQUEST': result = parseRemediationRequest(value, path); break
    case 'REMEDIATION_REPORT': result = parseRemediationReport(value, path); break
    case 'DELIVERY_RECEIPT': result = parseDeliveryReceipt(value, path); break
    case 'EXECUTION_CHANGE_ANNOTATION':
      result = parseExecutionChangeAnnotation(value, path)
      break
  }
  return result as StrongFlowArtifactPayloadByKind[Kind]
}

function parseArtifactId<Kind extends StrongFlowArtifactKind>(
  kind: Kind,
  value: unknown,
  path: string,
): StrongFlowArtifactIdByKind[Kind] {
  const id = portableIdentifier(value, path)
  let result: string
  switch (kind) {
    case 'USER_REQUEST': result = UserRequestId(id); break
    case 'REQUIREMENT_SPEC': result = id as RequirementId; break
    case 'SOLUTION_DESIGN': result = id as SolutionId; break
    case 'SYSTEM_ARCHITECTURE_DIAGRAM':
    case 'PROCESS_FLOW_DIAGRAM': result = id as DiagramId; break
    case 'HUMAN_REVIEW_RECORD': result = id as HumanReviewId; break
    case 'EXECUTION_PLAN': result = ExecutionPlanId(id); break
    case 'PATCH_MANIFEST': result = PatchManifestId(id); break
    case 'REVIEW_REPORT': result = ReviewReportId(id); break
    case 'VERIFICATION_REPORT': result = VerificationReportId(id); break
    case 'REMEDIATION_REQUEST': result = RemediationRequestId(id); break
    case 'REMEDIATION_REPORT': result = RemediationReportId(id); break
    case 'DELIVERY_RECEIPT': result = DeliveryReceiptId(id); break
    case 'EXECUTION_CHANGE_ANNOTATION': result = ExecutionChangeAnnotationId(id); break
  }
  return result as StrongFlowArtifactIdByKind[Kind]
}

function parseReference(value: unknown, path: string): StrongFlowArtifactReference {
  const input = record(value, path)
  exactKeys(input, ['artifactKind', 'artifactId'], path)
  const kind = enumValue(input.artifactKind, STRONGFLOW_ARTIFACT_KINDS, `${path}.artifactKind`)
  return Object.freeze({
    artifactKind: kind,
    artifactId: parseArtifactId(kind, input.artifactId, `${path}.artifactId`),
  })
}

function parseReferences(value: unknown, path: string): readonly StrongFlowArtifactReference[] {
  const result = arrayOf(value, path, parseReference, { maxLength: 100 })
  uniqueIdentifiers(
    result.map(entry => `${entry.artifactKind}:${entry.artifactId}`),
    path,
  )
  return result
}

function parseProducer(value: unknown, path: string): StrongFlowArtifactProducer {
  const input = record(value, path)
  if (input.kind === 'role') {
    exactKeys(input, ['kind', 'roleId', 'stageRunId', 'attemptId'], path)
    return Object.freeze({
      kind: 'role',
      roleId: enumValue(input.roleId, STRONGFLOW_ROLE_IDS, `${path}.roleId`),
      stageRunId: portableIdentifier(input.stageRunId, `${path}.stageRunId`) as StageRunId,
      attemptId: portableIdentifier(input.attemptId, `${path}.attemptId`) as AttemptId,
    })
  }
  if (input.kind === 'human') {
    exactKeys(input, ['kind', 'actorId', 'channel'], path)
    return Object.freeze({
      kind: 'human',
      actorId: portableIdentifier(input.actorId, `${path}.actorId`),
      channel: enumValue(input.channel, ['local-ui', 'cli'] as const, `${path}.channel`),
    })
  }
  if (input.kind === 'system') {
    exactKeys(input, ['kind', 'actorId'], path)
    return Object.freeze({
      kind: 'system',
      actorId: portableIdentifier(input.actorId, `${path}.actorId`),
    })
  }
  artifactError('INVALID_PRODUCER', `${path}.kind`, 'artifact producer kind is unsupported')
}

export function parseStrongFlowArtifactKernelEventInterval(
  value: unknown,
  path = 'kernelEventInterval',
): StrongFlowArtifactKernelEventInterval | null {
  if (value === null) return null
  const input = record(value, path)
  exactKeys(input, [
    'schemaVersion',
    'kernelSessionLineageId',
    'contextId',
    'generation',
    'kernelSessionId',
    'kernelStreamId',
    'turnId',
    'firstSequence',
    'lastSequence',
    'eventCount',
  ], path)
  if (input.schemaVersion !== 1) {
    artifactError(
      'UNSUPPORTED_SCHEMA_VERSION',
      `${path}.schemaVersion`,
      'kernel event interval schema version is unsupported',
    )
  }
  const firstSequence = portableIdentifier(input.firstSequence, `${path}.firstSequence`)
  const lastSequence = portableIdentifier(input.lastSequence, `${path}.lastSequence`)
  if (!DECIMAL_SEQUENCE_PATTERN.test(firstSequence)
    || !DECIMAL_SEQUENCE_PATTERN.test(lastSequence)) {
    artifactError(
      'INVALID_EVENT_INTERVAL',
      path,
      'kernel event interval sequences must be canonical decimal strings',
    )
  }
  const eventCount = positiveInteger(input.eventCount, `${path}.eventCount`)
  const first = BigInt(firstSequence)
  const last = BigInt(lastSequence)
  if (last < first || last - first + 1n !== BigInt(eventCount)) {
    artifactError(
      'INVALID_EVENT_INTERVAL',
      path,
      'kernel event interval count does not match its inclusive sequence range',
    )
  }
  return Object.freeze({
    schemaVersion: 1,
    kernelSessionLineageId: portableIdentifier(
      input.kernelSessionLineageId,
      `${path}.kernelSessionLineageId`,
    ),
    contextId: portableIdentifier(input.contextId, `${path}.contextId`),
    generation: positiveInteger(input.generation, `${path}.generation`),
    kernelSessionId: portableIdentifier(
      input.kernelSessionId,
      `${path}.kernelSessionId`,
    ) as KernelSessionId,
    kernelStreamId: portableIdentifier(input.kernelStreamId, `${path}.kernelStreamId`),
    turnId: portableIdentifier(input.turnId, `${path}.turnId`),
    firstSequence,
    lastSequence,
    eventCount,
  })
}

const DEFINITION_SOURCE_KINDS = Object.freeze([
  'REQUIREMENT_SPEC',
  'SOLUTION_DESIGN',
  'SYSTEM_ARCHITECTURE_DIAGRAM',
  'PROCESS_FLOW_DIAGRAM',
] as const)

const APPROVED_DEFINITION_SOURCE_KINDS = Object.freeze([
  ...DEFINITION_SOURCE_KINDS,
  'HUMAN_REVIEW_RECORD',
] as const)

const CANDIDATE_SOURCE_KINDS = Object.freeze([
  ...APPROVED_DEFINITION_SOURCE_KINDS,
  'EXECUTION_PLAN',
  'PATCH_MANIFEST',
] as const)

function expectedSourceKinds(
  kind: StrongFlowArtifactKind,
  producer: StrongFlowArtifactProducer,
  sources: readonly StrongFlowArtifactReference[],
): readonly StrongFlowArtifactKind[] {
  switch (kind) {
    case 'USER_REQUEST': return []
    case 'REQUIREMENT_SPEC': return ['USER_REQUEST']
    case 'SOLUTION_DESIGN': return ['REQUIREMENT_SPEC']
    case 'SYSTEM_ARCHITECTURE_DIAGRAM':
    case 'PROCESS_FLOW_DIAGRAM': return ['REQUIREMENT_SPEC', 'SOLUTION_DESIGN']
    case 'HUMAN_REVIEW_RECORD': return DEFINITION_SOURCE_KINDS
    case 'EXECUTION_PLAN': return APPROVED_DEFINITION_SOURCE_KINDS
    case 'PATCH_MANIFEST':
      return producer.kind === 'role' && producer.roleId === 'remediator'
        ? [
          ...CANDIDATE_SOURCE_KINDS,
          'REVIEW_REPORT',
          'VERIFICATION_REPORT',
          'REMEDIATION_REQUEST',
        ]
        : [...APPROVED_DEFINITION_SOURCE_KINDS, 'EXECUTION_PLAN']
    case 'REVIEW_REPORT': return [...CANDIDATE_SOURCE_KINDS]
    case 'VERIFICATION_REPORT':
      return producer.kind === 'role' && producer.roleId === 'adversarial-verifier'
        ? [...CANDIDATE_SOURCE_KINDS, 'REVIEW_REPORT', 'VERIFICATION_REPORT']
        : [...CANDIDATE_SOURCE_KINDS, 'REVIEW_REPORT']
    case 'REMEDIATION_REQUEST': {
      const annotations = sources.filter(
        source => source.artifactKind === 'EXECUTION_CHANGE_ANNOTATION',
      )
      return [
        ...CANDIDATE_SOURCE_KINDS,
        'REVIEW_REPORT',
        'VERIFICATION_REPORT',
        ...annotations.map(() => 'EXECUTION_CHANGE_ANNOTATION' as const),
      ]
    }
    case 'REMEDIATION_REPORT':
      return [
        ...CANDIDATE_SOURCE_KINDS,
        'REVIEW_REPORT',
        'VERIFICATION_REPORT',
        'REMEDIATION_REQUEST',
        'PATCH_MANIFEST',
      ]
    case 'DELIVERY_RECEIPT': {
      const extra = sources.slice(9).map(source => source.artifactKind)
      return [
        ...CANDIDATE_SOURCE_KINDS,
        'REVIEW_REPORT',
        'VERIFICATION_REPORT',
        ...extra,
      ]
    }
    case 'EXECUTION_CHANGE_ANNOTATION': {
      const diagramKind = sources[0]?.artifactKind
      if (diagramKind !== 'SYSTEM_ARCHITECTURE_DIAGRAM'
        && diagramKind !== 'PROCESS_FLOW_DIAGRAM') {
        return ['SYSTEM_ARCHITECTURE_DIAGRAM', 'PATCH_MANIFEST']
      }
      return [diagramKind, 'PATCH_MANIFEST']
    }
  }
}

function validateProducer(
  kind: StrongFlowArtifactKind,
  producer: StrongFlowArtifactProducer,
  interval: StrongFlowArtifactKernelEventInterval | null,
): void {
  const roleByArtifact: Partial<Record<StrongFlowArtifactKind, readonly StrongFlowRoleId[]>> = {
    REQUIREMENT_SPEC: ['requirements'],
    SOLUTION_DESIGN: ['solution'],
    SYSTEM_ARCHITECTURE_DIAGRAM: ['solution'],
    PROCESS_FLOW_DIAGRAM: ['solution'],
    EXECUTION_PLAN: ['planner'],
    PATCH_MANIFEST: ['executor', 'remediator'],
    REVIEW_REPORT: ['reviewer'],
    VERIFICATION_REPORT: ['verifier', 'adversarial-verifier'],
    REMEDIATION_REPORT: ['remediator'],
  }
  const allowedRoles = roleByArtifact[kind]
  if (allowedRoles !== undefined) {
    if (producer.kind !== 'role' || !allowedRoles.includes(producer.roleId)) {
      artifactError('INVALID_PRODUCER', 'artifact.producer', `${kind} has an invalid role producer`)
    }
  } else if (kind === 'HUMAN_REVIEW_RECORD' || kind === 'EXECUTION_CHANGE_ANNOTATION') {
    if (producer.kind !== 'human') {
      artifactError('INVALID_PRODUCER', 'artifact.producer', `${kind} requires a human producer`)
    }
  } else if (kind === 'REMEDIATION_REQUEST' || kind === 'DELIVERY_RECEIPT') {
    if (producer.kind !== 'system') {
      artifactError('INVALID_PRODUCER', 'artifact.producer', `${kind} requires a system producer`)
    }
  } else if (kind === 'USER_REQUEST' && producer.kind === 'role') {
    artifactError('INVALID_PRODUCER', 'artifact.producer', 'USER_REQUEST cannot come from a role')
  }
  if ((producer.kind === 'role') !== (interval !== null)) {
    artifactError(
      'INVALID_EVENT_INTERVAL',
      'artifact.kernelEventInterval',
      'role artifacts require an event interval and non-role artifacts require null',
    )
  }
}

function validateSourceKinds(
  kind: StrongFlowArtifactKind,
  producer: StrongFlowArtifactProducer,
  sources: readonly StrongFlowArtifactReference[],
): void {
  const expected = expectedSourceKinds(kind, producer, sources)
  const actual = sources.map(source => source.artifactKind)
  if (actual.length !== expected.length
    || expected.some((entry, index) => actual[index] !== entry)) {
    artifactError(
      'INVALID_SOURCE_ARTIFACTS',
      'artifact.sourceArtifacts',
      `${kind} does not reference its exact ordered source artifacts`,
    )
  }
  if (kind === 'DELIVERY_RECEIPT') {
    const extras = actual.slice(9)
    if (extras.some(entry => entry !== 'VERIFICATION_REPORT' && entry !== 'REMEDIATION_REPORT')) {
      artifactError(
        'INVALID_SOURCE_ARTIFACTS',
        'artifact.sourceArtifacts',
        'DELIVERY_RECEIPT contains an unsupported source artifact',
      )
    }
  }
}

function sourceId(
  sources: readonly StrongFlowArtifactReference[],
  kind: StrongFlowArtifactKind,
  occurrence = 0,
): string | undefined {
  return sources.filter(source => source.artifactKind === kind)[occurrence]?.artifactId
}

function requireSourceId(
  sources: readonly StrongFlowArtifactReference[],
  kind: StrongFlowArtifactKind,
  id: string,
  path: string,
  occurrence = 0,
): void {
  if (sourceId(sources, kind, occurrence) !== id) {
    artifactError('INVALID_RELATIONSHIP', path, `${path} does not match its source artifact`)
  }
}

function validateDefinitionSources(
  sources: readonly StrongFlowArtifactReference[],
  definition: DefinitionIdentity,
  path: string,
): void {
  requireSourceId(sources, 'REQUIREMENT_SPEC', definition.requirementId, `${path}.requirementId`)
  requireSourceId(sources, 'SOLUTION_DESIGN', definition.solutionId, `${path}.solutionId`)
  requireSourceId(
    sources,
    'SYSTEM_ARCHITECTURE_DIAGRAM',
    definition.systemArchitectureDiagramId,
    `${path}.systemArchitectureDiagramId`,
  )
  requireSourceId(
    sources,
    'PROCESS_FLOW_DIAGRAM',
    definition.processFlowDiagramId,
    `${path}.processFlowDiagramId`,
  )
}

function validateArtifactRelationships(artifact: StrongFlowArtifact): void {
  const { artifactKind: kind, sourceArtifacts: sources, payload } = artifact
  switch (kind) {
    case 'USER_REQUEST':
    case 'REQUIREMENT_SPEC': return
    case 'SOLUTION_DESIGN':
      requireSourceId(sources, 'REQUIREMENT_SPEC', payload.requirementId, 'payload.requirementId')
      return
    case 'SYSTEM_ARCHITECTURE_DIAGRAM':
    case 'PROCESS_FLOW_DIAGRAM':
      requireSourceId(sources, 'REQUIREMENT_SPEC', payload.requirementId, 'payload.requirementId')
      requireSourceId(sources, 'SOLUTION_DESIGN', payload.solutionId, 'payload.solutionId')
      return
    case 'HUMAN_REVIEW_RECORD':
      validateDefinitionSources(sources, payload.definition, 'payload.definition')
      return
    case 'EXECUTION_PLAN':
      validateDefinitionSources(sources, payload.definition, 'payload.definition')
      requireSourceId(
        sources,
        'HUMAN_REVIEW_RECORD',
        payload.humanReviewId,
        'payload.humanReviewId',
      )
      return
    case 'PATCH_MANIFEST':
      requireSourceId(sources, 'EXECUTION_PLAN', payload.executionPlanId, 'payload.executionPlanId')
      if (payload.remediationRequestId !== null) {
        requireSourceId(
          sources,
          'REMEDIATION_REQUEST',
          payload.remediationRequestId,
          'payload.remediationRequestId',
        )
      }
      return
    case 'REVIEW_REPORT':
      requireSourceId(sources, 'PATCH_MANIFEST', payload.patchManifestId, 'payload.patchManifestId')
      return
    case 'VERIFICATION_REPORT':
      requireSourceId(sources, 'PATCH_MANIFEST', payload.patchManifestId, 'payload.patchManifestId')
      if ((artifact.producer as { readonly roleId?: string }).roleId === 'verifier'
        && payload.mode !== 'standard') {
        artifactError('INVALID_RELATIONSHIP', 'payload.mode', 'verifier must use standard mode')
      }
      if ((artifact.producer as { readonly roleId?: string }).roleId === 'adversarial-verifier'
        && payload.mode !== 'adversarial') {
        artifactError('INVALID_RELATIONSHIP', 'payload.mode', 'adversarial verifier mode is required')
      }
      return
    case 'REMEDIATION_REQUEST':
      requireSourceId(sources, 'PATCH_MANIFEST', payload.patchManifestId, 'payload.patchManifestId')
      for (const finding of payload.findings) {
        if (!sources.some(source => (
          source.artifactKind === finding.sourceArtifactKind
          && source.artifactId === finding.sourceArtifactId
        ))) {
          artifactError(
            'INVALID_RELATIONSHIP',
            'payload.findings',
            'remediation finding does not match a source report',
          )
        }
      }
      for (const annotationId of payload.annotationIds) {
        requireSourceId(
          sources,
          'EXECUTION_CHANGE_ANNOTATION',
          annotationId,
          'payload.annotationIds',
          payload.annotationIds.indexOf(annotationId),
        )
      }
      return
    case 'REMEDIATION_REPORT':
      requireSourceId(
        sources,
        'REMEDIATION_REQUEST',
        payload.remediationRequestId,
        'payload.remediationRequestId',
      )
      requireSourceId(
        sources,
        'PATCH_MANIFEST',
        payload.patchManifestId,
        'payload.patchManifestId',
        1,
      )
      return
    case 'DELIVERY_RECEIPT':
      validateDefinitionSources(sources, payload.definition, 'payload.definition')
      requireSourceId(sources, 'HUMAN_REVIEW_RECORD', payload.humanReviewId, 'payload.humanReviewId')
      requireSourceId(sources, 'EXECUTION_PLAN', payload.executionPlanId, 'payload.executionPlanId')
      requireSourceId(sources, 'PATCH_MANIFEST', payload.patchManifestId, 'payload.patchManifestId')
      requireSourceId(sources, 'REVIEW_REPORT', payload.reviewReportId, 'payload.reviewReportId')
      for (const [index, reportId] of payload.verificationReportIds.entries()) {
        requireSourceId(
          sources,
          'VERIFICATION_REPORT',
          reportId,
          'payload.verificationReportIds',
          index,
        )
      }
      if (payload.remediationReportId !== null) {
        requireSourceId(
          sources,
          'REMEDIATION_REPORT',
          payload.remediationReportId,
          'payload.remediationReportId',
        )
      }
      return
    case 'EXECUTION_CHANGE_ANNOTATION':
      requireSourceId(sources, payload.diagramKind, payload.diagramId, 'payload.diagramId')
      requireSourceId(sources, 'PATCH_MANIFEST', payload.patchManifestId, 'payload.patchManifestId')
  }
}

/** The single full-artifact parser used at every trusted/untrusted boundary. */
export function parseStrongFlowArtifact(value: unknown): StrongFlowArtifact {
  const input = record(value, 'artifact')
  exactKeys(input, [
    'schemaVersion',
    'artifactKind',
    'artifactId',
    'jobId',
    'sourceArtifacts',
    'producer',
    'kernelEventInterval',
    'createdAtMillis',
    'payload',
  ], 'artifact')
  if (input.schemaVersion !== STRONGFLOW_ARTIFACT_SCHEMA_VERSION) {
    artifactError(
      'UNSUPPORTED_SCHEMA_VERSION',
      'artifact.schemaVersion',
      'artifact schema version is unsupported',
    )
  }
  const kind = enumValue(
    input.artifactKind,
    STRONGFLOW_ARTIFACT_KINDS,
    'artifact.artifactKind',
  )
  const producer = parseProducer(input.producer, 'artifact.producer')
  const interval = parseStrongFlowArtifactKernelEventInterval(
    input.kernelEventInterval,
    'artifact.kernelEventInterval',
  )
  const artifact = Object.freeze({
    schemaVersion: STRONGFLOW_ARTIFACT_SCHEMA_VERSION,
    artifactKind: kind,
    artifactId: parseArtifactId(kind, input.artifactId, 'artifact.artifactId'),
    jobId: portableIdentifier(input.jobId, 'artifact.jobId') as JobId,
    sourceArtifacts: parseReferences(input.sourceArtifacts, 'artifact.sourceArtifacts'),
    producer,
    kernelEventInterval: interval,
    createdAtMillis: nonNegativeInteger(input.createdAtMillis, 'artifact.createdAtMillis'),
    payload: parseStrongFlowArtifactPayload(kind, input.payload),
  }) as StrongFlowArtifact
  validateProducer(kind, producer, interval)
  validateSourceKinds(kind, producer, artifact.sourceArtifacts)
  validateArtifactRelationships(artifact)
  return artifact
}

export function parseStrongFlowArtifactAs<Kind extends StrongFlowArtifactKind>(
  kind: Kind,
  value: unknown,
): StrongFlowArtifactFor<Kind> {
  const artifact = parseStrongFlowArtifact(value)
  if (artifact.artifactKind !== kind) {
    artifactError(
      'ARTIFACT_KIND_MISMATCH',
      'artifact.artifactKind',
      `expected ${kind}, received ${artifact.artifactKind}`,
    )
  }
  return artifact as StrongFlowArtifactFor<Kind>
}

/** Adds program-owned identity and provenance after validating the model-owned payload. */
export function materializeStrongFlowArtifact<Kind extends StrongFlowArtifactKind>(
  kind: Kind,
  metadata: StrongFlowArtifactMetadata<Kind>,
  payload: unknown,
): StrongFlowArtifactFor<Kind> {
  const validatedPayload = parseStrongFlowArtifactPayload(kind, payload)
  return parseStrongFlowArtifactAs(kind, {
    schemaVersion: STRONGFLOW_ARTIFACT_SCHEMA_VERSION,
    artifactKind: kind,
    ...metadata,
    payload: validatedPayload,
  })
}

export interface StrongFlowExecutionAnnotationTarget {
  readonly candidate: StrongFlowCandidateIdentity
  readonly patchManifestId: PatchManifestId
  readonly diagramKind: 'SYSTEM_ARCHITECTURE_DIAGRAM' | 'PROCESS_FLOW_DIAGRAM'
  readonly diagramId: DiagramId
  readonly nodeIds: readonly DiagramNodeId[]
  readonly hunks: readonly { readonly path: string; readonly hunkId: string }[]
}

function parseExecutionAnnotationTarget(
  value: unknown,
): StrongFlowExecutionAnnotationTarget {
  const input = record(value, 'annotationTarget')
  exactKeys(input, [
    'candidate',
    'patchManifestId',
    'diagramKind',
    'diagramId',
    'nodeIds',
    'hunks',
  ], 'annotationTarget')
  const hunks = arrayOf(
    input.hunks,
    'annotationTarget.hunks',
    (entry, path) => {
      const hunk = record(entry, path)
      exactKeys(hunk, ['path', 'hunkId'], path)
      return Object.freeze({
        path: relativePath(hunk.path, `${path}.path`),
        hunkId: portableIdentifier(hunk.hunkId, `${path}.hunkId`),
      })
    },
  )
  uniqueIdentifiers(hunks.map(hunk => `${hunk.path}:${hunk.hunkId}`), 'annotationTarget.hunks')
  return Object.freeze({
    candidate: parseStrongFlowCandidateIdentity(input.candidate, 'annotationTarget.candidate'),
    patchManifestId: PatchManifestId(portableIdentifier(
      input.patchManifestId,
      'annotationTarget.patchManifestId',
    )),
    diagramKind: enumValue(
      input.diagramKind,
      ['SYSTEM_ARCHITECTURE_DIAGRAM', 'PROCESS_FLOW_DIAGRAM'] as const,
      'annotationTarget.diagramKind',
    ),
    diagramId: portableIdentifier(input.diagramId, 'annotationTarget.diagramId') as DiagramId,
    nodeIds: identifierArray(input.nodeIds, 'annotationTarget.nodeIds') as readonly DiagramNodeId[],
    hunks,
  })
}

/** Rejects annotations made against an older candidate, diff, diagram, node, file, or hunk. */
export function requireCurrentExecutionChangeAnnotation(
  annotationValue: unknown,
  targetValue: unknown,
): ExecutionChangeAnnotation {
  const annotation = parseStrongFlowArtifactAs('EXECUTION_CHANGE_ANNOTATION', annotationValue)
  const target = parseExecutionAnnotationTarget(targetValue)
  const payload = annotation.payload
  if (
    payload.candidateId !== target.candidate.candidateId
    || payload.diffId !== target.candidate.diffId
    || payload.patchManifestId !== target.patchManifestId
    || payload.diagramKind !== target.diagramKind
    || payload.diagramId !== target.diagramId
    || !target.nodeIds.includes(payload.nodeId)
  ) {
    artifactError(
      'STALE_ANNOTATION_TARGET',
      'annotation.payload',
      'annotation does not match the current candidate diagram node',
    )
  }
  if (payload.location !== null && !target.hunks.some(hunk => (
    hunk.path === payload.location?.path && hunk.hunkId === payload.location.hunkId
  ))) {
    artifactError(
      'STALE_ANNOTATION_TARGET',
      'annotation.payload.location',
      'annotation does not match a current file hunk',
    )
  }
  return annotation
}
