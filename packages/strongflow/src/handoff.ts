import { createHash } from 'node:crypto'
import { isDeepStrictEqual } from 'node:util'

import {
  AttemptId,
  StageRunId,
  STRONGFLOW_HANDOFF_MAX_CONTEXT_BYTES,
  STRONGFLOW_ROLE_IDS,
  materializeStrongFlowHandoffManifest,
  parseStrongFlowArtifactAs,
  strongFlowRoleAcceptedInputArtifacts,
  type AttemptId as AttemptIdentifier,
  type DefinitionIdentity,
  type HumanReviewRecord,
  type LosslessJsonValue,
  type StageRunId as StageRunIdentifier,
  type StrongFlowArtifact,
  type StrongFlowArtifactKind,
  type StrongFlowArtifactReference,
  type StrongFlowCandidateIdentity,
  type StrongFlowHandoffId,
  type StrongFlowHandoffInputReference,
  type StrongFlowHandoffManifest,
  type StrongFlowJobEvent,
  type StrongFlowJobSnapshot,
  type StrongFlowJobStage,
  type StrongFlowRoleArtifactKind,
  type StrongFlowRoleId,
  type VerificationReport,
} from '@winwincode/contracts'

import {
  StrongFlowArtifactStore,
  StrongFlowArtifactStoreError,
  type StrongFlowArtifactStoreArtifactRecord,
  type StrongFlowArtifactStoreHandoffRecord,
  type StrongFlowStoredArtifactContent,
} from './artifact-store.js'
import { validateStrongFlowDefinitionDiagramPair } from './definition-diagrams.js'
import { StrongFlowJobStore, StrongFlowJobStoreError } from './job-store.js'
import {
  STRONGFLOW_ROLE_RUNNER_DEFAULT_MAX_INPUT_BYTES,
  type StrongFlowIdentifiedRoleArtifact,
} from './role-runner.js'

const MAX_ARTIFACT_RECORD_SCAN = 10_000

const ROLE_STAGE: Readonly<Record<StrongFlowRoleId, StrongFlowJobStage>> = Object.freeze({
  requirements: 'REQUIREMENTS',
  solution: 'SOLUTION',
  planner: 'PLANNING',
  executor: 'EXECUTION',
  reviewer: 'VERIFICATION',
  verifier: 'VERIFICATION',
  'adversarial-verifier': 'VERIFICATION',
  remediator: 'REMEDIATION',
})

export type StrongFlowHandoffErrorCode =
  | 'INVALID_HANDOFF_OPTIONS'
  | 'INVALID_HANDOFF_REQUEST'
  | 'JOB_ID_MISMATCH'
  | 'WRONG_JOB_STATE'
  | 'STAGE_RUN_MISMATCH'
  | 'ARTIFACT_NOT_FOUND'
  | 'ARTIFACT_AMBIGUOUS'
  | 'ARTIFACT_CHAIN_MISMATCH'
  | 'APPROVAL_REQUIRED'
  | 'STALE_DEFINITION'
  | 'STALE_CANDIDATE'
  | 'CONTEXT_LIMIT_EXCEEDED'
  | 'HANDOFF_NOT_FOUND'
  | 'HANDOFF_CORRUPT'
  | 'STORE_FAILURE'

export class StrongFlowHandoffError extends Error {
  readonly code: StrongFlowHandoffErrorCode

  constructor(
    code: StrongFlowHandoffErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowHandoffError'
    this.code = code
  }
}

export interface StrongFlowHandoffBuilderOptions {
  readonly artifactStore: StrongFlowArtifactStore
  readonly jobStore: StrongFlowJobStore
  readonly systemActorId?: string
  readonly contextLimitBytes?: number
}

export interface BuildStrongFlowRoleHandoffRequest {
  readonly roleId: StrongFlowRoleId
  readonly stageRunId: StageRunIdentifier
  readonly attemptId: AttemptIdentifier
}

export interface StrongFlowBuiltHandoff {
  readonly handoff: StrongFlowHandoffManifest
  readonly record: StrongFlowArtifactStoreHandoffRecord
  /** These are the exact ordered values to pass to StrongFlowRoleRunner.run. */
  readonly inputs: readonly StrongFlowIdentifiedRoleArtifact[]
}

interface StageSuccessIdentity {
  readonly stage: StrongFlowJobStage
  readonly stageRunId: StageRunIdentifier
  readonly attemptId: AttemptIdentifier
  readonly candidateId?: string
  readonly kernelSessionId?: string
}

interface ApprovedDefinitionSelection {
  readonly definition: DefinitionIdentity
  readonly approval: HumanReviewRecord
  readonly artifacts: readonly StrongFlowStoredArtifactContent[]
}

function handoffError(
  code: StrongFlowHandoffErrorCode,
  message: string,
  options?: ErrorOptions,
): never {
  throw new StrongFlowHandoffError(code, message, options)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function exactKeys(value: Record<string, unknown>, keys: readonly string[]): void {
  const expected = new Set(keys)
  if (
    Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))
  ) handoffError('INVALID_HANDOFF_REQUEST', 'role handoff request has an unexpected shape')
}

function parseRoleRequest(value: unknown): BuildStrongFlowRoleHandoffRequest {
  if (!isRecord(value)) {
    handoffError('INVALID_HANDOFF_REQUEST', 'role handoff request must be an object')
  }
  exactKeys(value, ['roleId', 'stageRunId', 'attemptId'])
  if (typeof value.roleId !== 'string' || !STRONGFLOW_ROLE_IDS.includes(
    value.roleId as StrongFlowRoleId,
  )) handoffError('INVALID_HANDOFF_REQUEST', 'role handoff request has an invalid role')
  try {
    return Object.freeze({
      roleId: value.roleId as StrongFlowRoleId,
      stageRunId: StageRunId(String(value.stageRunId)),
      attemptId: AttemptId(String(value.attemptId)),
    })
  } catch (error) {
    handoffError('INVALID_HANDOFF_REQUEST', 'role handoff request has invalid run identities', {
      cause: error,
    })
  }
}

function completeDefinition(snapshot: StrongFlowJobSnapshot): DefinitionIdentity | undefined {
  const definition = snapshot.definition
  if (
    definition.requirementId === undefined
    || definition.solutionId === undefined
    || definition.systemArchitectureDiagramId === undefined
    || definition.processFlowDiagramId === undefined
  ) return undefined
  return Object.freeze({
    requirementId: definition.requirementId,
    solutionId: definition.solutionId,
    systemArchitectureDiagramId: definition.systemArchitectureDiagramId,
    processFlowDiagramId: definition.processFlowDiagramId,
  })
}

function sameDefinition(left: DefinitionIdentity, right: DefinitionIdentity): boolean {
  return left.requirementId === right.requirementId
    && left.solutionId === right.solutionId
    && left.systemArchitectureDiagramId === right.systemArchitectureDiagramId
    && left.processFlowDiagramId === right.processFlowDiagramId
}

function latestStageSuccess(
  events: readonly StrongFlowJobEvent[],
  stages: readonly StrongFlowJobStage[],
): StageSuccessIdentity | undefined {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index]
    if (event?.kind !== 'stage.succeeded' || !stages.includes(event.data.stage)) continue
    return Object.freeze({
      stage: event.data.stage,
      stageRunId: event.data.stageRunId,
      attemptId: event.data.attemptId,
      ...('candidateId' in event.data ? { candidateId: event.data.candidateId } : {}),
      ...(event.source.kind === 'role' && event.source.kernelSessionId !== undefined
        ? { kernelSessionId: event.source.kernelSessionId }
        : {}),
    })
  }
  return undefined
}

function artifactKinds(artifacts: readonly StrongFlowStoredArtifactContent[]): string[] {
  return artifacts.map(entry => entry.artifact.artifactKind)
}

function refsEqual(
  references: readonly StrongFlowArtifactReference[],
  artifacts: readonly StrongFlowStoredArtifactContent[],
): boolean {
  return references.length === artifacts.length
    && references.every((reference, index) => (
      reference.artifactKind === artifacts[index]?.artifact.artifactKind
      && reference.artifactId === artifacts[index]?.artifact.artifactId
    ))
}

function candidateFromArtifact(artifact: StrongFlowArtifact): StrongFlowCandidateIdentity | null {
  switch (artifact.artifactKind) {
    case 'PATCH_MANIFEST':
    case 'REVIEW_REPORT':
    case 'VERIFICATION_REPORT':
    case 'REMEDIATION_REQUEST':
    case 'REMEDIATION_REPORT':
    case 'DELIVERY_RECEIPT':
      return artifact.payload.candidate
    default:
      return null
  }
}

function toRoleInput(entry: StrongFlowStoredArtifactContent): StrongFlowIdentifiedRoleArtifact {
  if (entry.artifact.artifactKind === 'EXECUTION_CHANGE_ANNOTATION') {
    handoffError('ARTIFACT_CHAIN_MISMATCH', 'annotations are not model role input artifacts')
  }
  return Object.freeze({
    artifactId: entry.artifact.artifactId,
    kind: entry.artifact.artifactKind as StrongFlowRoleArtifactKind,
    value: entry.artifact as unknown as LosslessJsonValue,
  })
}

function handoffInputReference(
  entry: StrongFlowStoredArtifactContent,
  position: number,
): StrongFlowHandoffInputReference {
  return Object.freeze({
    position,
    artifactKind: entry.artifact.artifactKind,
    artifactId: entry.artifact.artifactId,
    artifactRecordId: entry.record.recordId,
    blobId: entry.record.blob.blobId,
    byteLength: entry.record.blob.byteLength,
  })
}

function handoffDigest(value: unknown): string {
  return createHash('sha256').update(JSON.stringify(value)).digest('hex')
}

/**
 * Pins every role or human-review input to immutable artifact-store records.
 * Selection uses durable job events and exact producer attempts, never "latest artifact".
 */
export class StrongFlowHandoffBuilder {
  readonly artifactStore: StrongFlowArtifactStore
  readonly jobStore: StrongFlowJobStore
  readonly systemActorId: string
  readonly contextLimitBytes: number

  constructor(options: StrongFlowHandoffBuilderOptions) {
    if (!isRecord(options)
      || !(options.artifactStore instanceof StrongFlowArtifactStore)
      || !(options.jobStore instanceof StrongFlowJobStore)) {
      handoffError(
        'INVALID_HANDOFF_OPTIONS',
        'handoff builder requires the artifact and job stores for one job',
      )
    }
    if (options.artifactStore.manifest.jobId !== options.jobStore.manifest.jobId) {
      handoffError('JOB_ID_MISMATCH', 'handoff stores do not belong to the same job')
    }
    const systemActorId = options.systemActorId ?? 'strongflow-handoff-builder'
    if (typeof systemActorId !== 'string'
      || !/^[A-Za-z0-9][A-Za-z0-9._:/-]{0,199}$/u.test(systemActorId)) {
      handoffError('INVALID_HANDOFF_OPTIONS', 'handoff system actor id is invalid')
    }
    const contextLimitBytes = options.contextLimitBytes
      ?? STRONGFLOW_ROLE_RUNNER_DEFAULT_MAX_INPUT_BYTES
    if (!Number.isSafeInteger(contextLimitBytes)
      || contextLimitBytes < 1
      || contextLimitBytes > STRONGFLOW_HANDOFF_MAX_CONTEXT_BYTES) {
      handoffError(
        'INVALID_HANDOFF_OPTIONS',
        `handoff context limit must be between 1 and ${STRONGFLOW_HANDOFF_MAX_CONTEXT_BYTES}`,
      )
    }
    this.artifactStore = options.artifactStore
    this.jobStore = options.jobStore
    this.systemActorId = systemActorId
    this.contextLimitBytes = contextLimitBytes
  }

  async buildHumanReview(): Promise<StrongFlowBuiltHandoff> {
    const stored = await this.#readJob()
    if (stored.snapshot.state !== 'AWAITING_HUMAN_REVIEW'
      || stored.snapshot.activeStage !== undefined
      || stored.snapshot.approval !== undefined) {
      handoffError('WRONG_JOB_STATE', 'human review handoff requires the pending definition state')
    }
    const approved = await this.#definitionArtifacts(stored)
    return this.#publish(
      stored.snapshot,
      Object.freeze({ kind: 'human-review' }),
      approved.definition,
      null,
      approved.artifacts,
    )
  }

  async buildRole(value: BuildStrongFlowRoleHandoffRequest): Promise<StrongFlowBuiltHandoff> {
    const request = parseRoleRequest(value)
    const stored = await this.#readJob()
    this.#assertActiveRole(stored.snapshot, request)
    const existing = await this.#existingRoleHandoff(request)
    if (existing !== undefined) {
      const reconstructed = await this.reconstruct(existing.identity.handoffId)
      if (reconstructed.handoff.modelInputBytes > this.contextLimitBytes) {
        handoffError('CONTEXT_LIMIT_EXCEEDED', 'stored handoff artifacts exceed the context limit')
      }
      return reconstructed
    }
    const selection = await this.#roleArtifacts(request.roleId, stored)
    return this.#publish(
      stored.snapshot,
      Object.freeze({
        kind: 'role',
        roleId: request.roleId,
        stageRunId: request.stageRunId,
        attemptId: request.attemptId,
      }),
      selection.definition,
      selection.candidate,
      selection.artifacts,
    )
  }

  async reconstruct(handoffId: StrongFlowHandoffId | string): Promise<StrongFlowBuiltHandoff> {
    let stored
    try {
      stored = await this.artifactStore.findHandoff(String(handoffId))
    } catch (error) {
      this.#storeFailure('stored handoff could not be read', error)
    }
    if (stored === undefined) handoffError('HANDOFF_NOT_FOUND', 'stored handoff was not found')
    const artifacts = await this.#loadInputReferences(stored.handoff.inputs)
    this.#validateSelectedKinds(stored.handoff.target, artifacts)
    const modelInputBytes = artifacts.reduce(
      (total, entry) => total + entry.record.blob.byteLength,
      0,
    )
    if (modelInputBytes !== stored.handoff.modelInputBytes) {
      handoffError('HANDOFF_CORRUPT', 'stored handoff byte accounting changed')
    }
    const expectedId = this.#handoffId({
      jobId: stored.handoff.jobId,
      jobSequence: stored.handoff.jobSequence,
      target: stored.handoff.target,
      definition: stored.handoff.definition,
      candidate: stored.handoff.candidate,
      inputs: stored.handoff.inputs,
      modelInputBytes: stored.handoff.modelInputBytes,
      contextLimitBytes: stored.handoff.contextLimitBytes,
      producer: stored.handoff.producer,
      createdAtMillis: stored.handoff.createdAtMillis,
    })
    if (expectedId !== stored.handoff.handoffId) {
      handoffError('HANDOFF_CORRUPT', 'stored handoff identity does not match its pinned inputs')
    }
    return Object.freeze({
      handoff: stored.handoff,
      record: stored.record,
      inputs: Object.freeze(artifacts.map(toRoleInput)),
    })
  }

  async #readJob(): Promise<Awaited<ReturnType<StrongFlowJobStore['read']>>> {
    try {
      const stored = await this.jobStore.read()
      if (stored.snapshot.jobId !== this.artifactStore.manifest.jobId) {
        handoffError('JOB_ID_MISMATCH', 'job state does not match the artifact store')
      }
      return stored
    } catch (error) {
      if (error instanceof StrongFlowHandoffError) throw error
      this.#storeFailure('durable job state could not be read', error)
    }
  }

  #assertActiveRole(
    snapshot: StrongFlowJobSnapshot,
    request: BuildStrongFlowRoleHandoffRequest,
  ): void {
    const expectedStage = ROLE_STAGE[request.roleId]
    const active = snapshot.activeStage
    if (active === undefined || active.stage !== expectedStage) {
      handoffError('WRONG_JOB_STATE', `role ${request.roleId} is not active for this job state`)
    }
    if (active.stageRunId !== request.stageRunId || active.attemptId !== request.attemptId) {
      handoffError('STAGE_RUN_MISMATCH', 'role handoff does not match the active stage attempt')
    }
  }

  async #roleArtifacts(
    roleId: StrongFlowRoleId,
    stored: Awaited<ReturnType<StrongFlowJobStore['read']>>,
  ): Promise<{
    readonly definition: DefinitionIdentity | null
    readonly candidate: StrongFlowCandidateIdentity | null
    readonly artifacts: readonly StrongFlowStoredArtifactContent[]
  }> {
    switch (roleId) {
      case 'requirements': {
        const request = await this.#uniqueArtifactByKind('USER_REQUEST')
        return Object.freeze({ definition: null, candidate: null, artifacts: [request] })
      }
      case 'solution': {
        const requirementId = stored.snapshot.definition.requirementId
        if (requirementId === undefined) {
          handoffError('STALE_DEFINITION', 'solution handoff has no current requirement')
        }
        const requirement = await this.#loadArtifact('REQUIREMENT_SPEC', requirementId)
        return Object.freeze({ definition: null, candidate: null, artifacts: [requirement] })
      }
      case 'planner': {
        const approved = await this.#approvedDefinitionArtifacts(stored)
        return Object.freeze({
          definition: approved.definition,
          candidate: null,
          artifacts: approved.artifacts,
        })
      }
      case 'executor': {
        const approved = await this.#approvedDefinitionArtifacts(stored)
        const plan = await this.#currentPlan(stored.events, approved)
        return Object.freeze({
          definition: approved.definition,
          candidate: null,
          artifacts: Object.freeze([plan]),
        })
      }
      case 'reviewer': {
        const patch = await this.#currentPatch(stored)
        return this.#candidateHandoffFromAnchor(stored, patch)
      }
      case 'verifier': {
        const active = stored.snapshot.activeStage
        if (active === undefined) handoffError('WRONG_JOB_STATE', 'verification stage is not active')
        const review = await this.#uniqueRoleArtifact(
          'REVIEW_REPORT',
          'reviewer',
          active.stageRunId,
          active.attemptId,
        )
        return this.#candidateHandoffFromAnchor(stored, review)
      }
      case 'adversarial-verifier': {
        const active = stored.snapshot.activeStage
        if (active === undefined) handoffError('WRONG_JOB_STATE', 'verification stage is not active')
        const verification = await this.#uniqueRoleArtifact(
          'VERIFICATION_REPORT',
          'verifier',
          active.stageRunId,
          active.attemptId,
        )
        const report = parseStrongFlowArtifactAs('VERIFICATION_REPORT', verification.artifact)
        if (report.payload.mode !== 'standard') {
          handoffError('ARTIFACT_CHAIN_MISMATCH', 'adversarial handoff requires a standard verification report')
        }
        return this.#candidateHandoffFromAnchor(stored, verification)
      }
      case 'remediator': {
        const request = await this.#currentRemediationRequest(stored)
        await this.#validateRemediationEvidence(request.artifact)
        return this.#candidateHandoffFromAnchor(stored, request)
      }
    }
  }

  async #definitionArtifacts(
    stored: Awaited<ReturnType<StrongFlowJobStore['read']>>,
  ): Promise<{
    readonly definition: DefinitionIdentity
    readonly artifacts: readonly StrongFlowStoredArtifactContent[]
  }> {
    const snapshot = stored.snapshot
    const definition = completeDefinition(snapshot)
    if (definition === undefined) {
      handoffError('STALE_DEFINITION', 'job has no complete current definition')
    }
    const artifacts = Object.freeze([
      await this.#loadArtifact('REQUIREMENT_SPEC', definition.requirementId),
      await this.#loadArtifact('SOLUTION_DESIGN', definition.solutionId),
      await this.#loadArtifact(
        'SYSTEM_ARCHITECTURE_DIAGRAM',
        definition.systemArchitectureDiagramId,
      ),
      await this.#loadArtifact('PROCESS_FLOW_DIAGRAM', definition.processFlowDiagramId),
    ])
    const requirement = artifacts[0]
    const solution = artifacts[1]
    const systemArchitectureDiagram = artifacts[2]
    const processFlowDiagram = artifacts[3]
    if (requirement === undefined
      || solution === undefined
      || systemArchitectureDiagram === undefined
      || processFlowDiagram === undefined) {
      handoffError('ARTIFACT_CHAIN_MISMATCH', 'current definition artifact set is incomplete')
    }
    const requirementsRun = latestStageSuccess(stored.events, ['REQUIREMENTS'])
    const solutionRun = latestStageSuccess(stored.events, ['SOLUTION'])
    if (requirementsRun === undefined || solutionRun === undefined) {
      handoffError('ARTIFACT_CHAIN_MISMATCH', 'definition has no durable producing stage runs')
    }
    this.#assertProducedByStage(requirement, 'requirements', requirementsRun)
    this.#assertProducedByStage(solution, 'solution', solutionRun)
    try {
      validateStrongFlowDefinitionDiagramPair({
        requirement: requirement.artifact,
        solution: solution.artifact,
        systemArchitectureDiagram: systemArchitectureDiagram.artifact,
        processFlowDiagram: processFlowDiagram.artifact,
      })
    } catch (error) {
      handoffError('ARTIFACT_CHAIN_MISMATCH', 'current definition diagrams are not the exact generated pair', {
        cause: error,
      })
    }
    return Object.freeze({ definition, artifacts })
  }

  async #approvedDefinitionArtifacts(
    stored: Awaited<ReturnType<StrongFlowJobStore['read']>>,
  ): Promise<ApprovedDefinitionSelection> {
    const current = await this.#definitionArtifacts(stored)
    const approval = stored.snapshot.approval
    if (approval === undefined || approval.payload.decision !== 'approved') {
      handoffError('APPROVAL_REQUIRED', 'current definition has no effective human approval')
    }
    if (!sameDefinition(approval.payload.definition, current.definition)) {
      handoffError('STALE_DEFINITION', 'human approval belongs to a different definition')
    }
    const storedApproval = await this.#loadArtifact('HUMAN_REVIEW_RECORD', approval.artifactId)
    if (!isDeepStrictEqual(storedApproval.artifact, approval)) {
      handoffError('ARTIFACT_CHAIN_MISMATCH', 'job approval does not match its durable artifact')
    }
    return Object.freeze({
      definition: current.definition,
      approval,
      artifacts: Object.freeze([...current.artifacts, storedApproval]),
    })
  }

  #assertProducedByStage(
    entry: StrongFlowStoredArtifactContent,
    roleId: StrongFlowRoleId,
    stage: StageSuccessIdentity,
  ): void {
    const producer = entry.record.producer
    if (producer.kind !== 'role'
      || producer.roleId !== roleId
      || producer.stageRunId !== stage.stageRunId
      || producer.attemptId !== stage.attemptId
      || (stage.kernelSessionId !== undefined
        && producer.eventInterval.kernelSessionId !== stage.kernelSessionId)) {
      handoffError(
        'ARTIFACT_CHAIN_MISMATCH',
        `${entry.artifact.artifactKind} does not match its durable producing stage run`,
      )
    }
  }

  async #currentPlan(
    events: readonly StrongFlowJobEvent[],
    approved: ApprovedDefinitionSelection,
  ): Promise<StrongFlowStoredArtifactContent> {
    const success = latestStageSuccess(events, ['PLANNING'])
    if (success === undefined) handoffError('ARTIFACT_NOT_FOUND', 'no successful planning run was recorded')
    const plan = await this.#uniqueRoleArtifact(
      'EXECUTION_PLAN',
      'planner',
      success.stageRunId,
      success.attemptId,
      undefined,
      success.kernelSessionId,
    )
    const artifact = parseStrongFlowArtifactAs('EXECUTION_PLAN', plan.artifact)
    if (!sameDefinition(artifact.payload.definition, approved.definition)
      || artifact.payload.humanReviewId !== approved.approval.artifactId
      || !refsEqual(artifact.sourceArtifacts, approved.artifacts)) {
      handoffError('STALE_DEFINITION', 'execution plan does not belong to the current approval')
    }
    return plan
  }

  async #currentPatch(
    stored: Awaited<ReturnType<StrongFlowJobStore['read']>>,
  ): Promise<StrongFlowStoredArtifactContent> {
    if (stored.snapshot.candidateId === undefined) {
      handoffError('STALE_CANDIDATE', 'candidate handoff has no current candidate')
    }
    const success = latestStageSuccess(stored.events, ['EXECUTION', 'REMEDIATION'])
    if (success === undefined || success.candidateId !== stored.snapshot.candidateId) {
      handoffError('STALE_CANDIDATE', 'current candidate has no matching successful writer run')
    }
    const roleId = success.stage === 'EXECUTION' ? 'executor' : 'remediator'
    const patch = await this.#uniqueRoleArtifact(
      'PATCH_MANIFEST',
      roleId,
      success.stageRunId,
      success.attemptId,
      entry => entry.record.candidate?.kind === 'complete'
        && entry.record.candidate.identity.candidateId === stored.snapshot.candidateId,
      success.kernelSessionId,
    )
    const artifact = parseStrongFlowArtifactAs('PATCH_MANIFEST', patch.artifact)
    if (artifact.payload.candidate.candidateId !== stored.snapshot.candidateId) {
      handoffError('STALE_CANDIDATE', 'patch manifest belongs to a stale candidate')
    }
    return patch
  }

  async #candidateHandoffFromAnchor(
    stored: Awaited<ReturnType<StrongFlowJobStore['read']>>,
    anchor: StrongFlowStoredArtifactContent,
  ): Promise<{
    readonly definition: DefinitionIdentity
    readonly candidate: StrongFlowCandidateIdentity
    readonly artifacts: readonly StrongFlowStoredArtifactContent[]
  }> {
    const modelReferences = anchor.artifact.artifactKind === 'REMEDIATION_REQUEST'
      ? anchor.artifact.sourceArtifacts.filter(
        reference => reference.artifactKind !== 'EXECUTION_CHANGE_ANNOTATION',
      )
      : anchor.artifact.sourceArtifacts
    const sources = await this.#loadReferences(modelReferences)
    const artifacts = Object.freeze([...sources, anchor])
    const expected = strongFlowRoleAcceptedInputArtifacts(
      anchor.artifact.artifactKind === 'PATCH_MANIFEST'
        ? 'reviewer'
        : anchor.artifact.artifactKind === 'REVIEW_REPORT'
          ? 'verifier'
          : anchor.artifact.artifactKind === 'VERIFICATION_REPORT'
            ? 'adversarial-verifier'
            : 'remediator',
    )
    if (artifactKinds(artifacts).length !== expected.length
      || expected.some((kind, index) => artifacts[index]?.artifact.artifactKind !== kind)) {
      handoffError('ARTIFACT_CHAIN_MISMATCH', 'candidate artifact sources do not form the required handoff')
    }
    const approved = await this.#approvedDefinitionArtifacts(stored)
    if (!artifacts.slice(0, 5).every((entry, index) => (
      entry.record.recordId === approved.artifacts[index]?.record.recordId
    ))) handoffError('STALE_DEFINITION', 'candidate handoff cites a stale approved definition')
    const plan = artifacts[5]
    if (plan === undefined || plan.artifact.artifactKind !== 'EXECUTION_PLAN') {
      handoffError('ARTIFACT_CHAIN_MISMATCH', 'candidate handoff has no execution plan')
    }
    const currentPlan = await this.#currentPlan(stored.events, approved)
    if (plan.record.recordId !== currentPlan.record.recordId) {
      handoffError('STALE_DEFINITION', 'candidate handoff cites a stale execution plan')
    }
    const candidate = candidateFromArtifact(anchor.artifact)
    if (candidate === null || stored.snapshot.candidateId !== candidate.candidateId) {
      handoffError('STALE_CANDIDATE', 'candidate handoff does not match the current job candidate')
    }
    for (const entry of artifacts.slice(6)) {
      const linked = candidateFromArtifact(entry.artifact)
      if (linked !== null && !isDeepStrictEqual(linked, candidate)) {
        handoffError('STALE_CANDIDATE', 'candidate handoff mixes different frozen candidates')
      }
    }
    return Object.freeze({ definition: approved.definition, candidate, artifacts })
  }

  async #currentRemediationRequest(
    stored: Awaited<ReturnType<StrongFlowJobStore['read']>>,
  ): Promise<StrongFlowStoredArtifactContent> {
    if (stored.snapshot.candidateId === undefined) {
      handoffError('STALE_CANDIDATE', 'remediation has no current candidate')
    }
    const verification = latestStageSuccess(stored.events, ['VERIFICATION'])
    if (verification === undefined
      || verification.candidateId !== stored.snapshot.candidateId) {
      handoffError('ARTIFACT_NOT_FOUND', 'remediation has no matching failed verification run')
    }
    const candidates = (await this.#artifactRecords()).filter(recordValue => (
      recordValue.identity.artifactKind === 'REMEDIATION_REQUEST'
      && recordValue.candidate?.kind === 'complete'
      && recordValue.candidate.identity.candidateId === stored.snapshot.candidateId
    ))
    const matches: StrongFlowStoredArtifactContent[] = []
    for (const recordValue of candidates) {
      const request = await this.#readArtifactRecord(recordValue)
      const reviewReference = request.artifact.sourceArtifacts.find(
        source => source.artifactKind === 'REVIEW_REPORT',
      )
      const verificationReference = request.artifact.sourceArtifacts.find(
        source => source.artifactKind === 'VERIFICATION_REPORT',
      )
      if (reviewReference === undefined || verificationReference === undefined) continue
      const reports = await Promise.all([
        this.#loadArtifact('REVIEW_REPORT', reviewReference.artifactId),
        this.#loadArtifact('VERIFICATION_REPORT', verificationReference.artifactId),
      ])
      if (reports.every(report => report.record.producer.kind === 'role'
        && report.record.producer.stageRunId === verification.stageRunId
        && report.record.producer.attemptId === verification.attemptId)) {
        matches.push(request)
      }
    }
    if (matches.length === 0) {
      handoffError('ARTIFACT_NOT_FOUND', 'no remediation request matches the failed verification run')
    }
    if (matches.length !== 1) {
      handoffError('ARTIFACT_AMBIGUOUS', 'more than one remediation request matches the failed verification run')
    }
    return matches[0]!
  }

  async #validateRemediationEvidence(artifactValue: StrongFlowArtifact): Promise<void> {
    const request = parseStrongFlowArtifactAs('REMEDIATION_REQUEST', artifactValue)
    for (const finding of request.payload.findings) {
      const source = await this.#loadArtifact(finding.sourceArtifactKind, finding.sourceArtifactId)
      if (source.artifact.artifactKind === 'REVIEW_REPORT') {
        const match = source.artifact.payload.findings.find(
          candidate => candidate.findingId === finding.findingId,
        )
        if (match === undefined || match.disposition !== 'open') {
          handoffError('ARTIFACT_CHAIN_MISMATCH', 'remediation request does not cite an open review finding')
        }
      } else {
        const report = source.artifact as VerificationReport
        const match = report.payload.checks.find(check => check.checkId === finding.findingId)
        if (match === undefined || match.outcome !== 'failed') {
          handoffError('ARTIFACT_CHAIN_MISMATCH', 'remediation request does not cite a failed verification check')
        }
      }
    }
  }

  async #uniqueArtifactByKind(
    kind: StrongFlowArtifactKind,
  ): Promise<StrongFlowStoredArtifactContent> {
    const matches = (await this.#artifactRecords()).filter(
      recordValue => recordValue.identity.artifactKind === kind,
    )
    if (matches.length === 0) handoffError('ARTIFACT_NOT_FOUND', `${kind} artifact was not found`)
    if (matches.length !== 1) handoffError('ARTIFACT_AMBIGUOUS', `${kind} artifact is ambiguous`)
    return this.#readArtifactRecord(matches[0]!)
  }

  async #uniqueRoleArtifact(
    kind: StrongFlowArtifactKind,
    roleId: StrongFlowRoleId,
    stageRunId: StageRunIdentifier,
    attemptId: AttemptIdentifier,
    predicate: (entry: StrongFlowStoredArtifactContent) => boolean = () => true,
    kernelSessionId?: string,
  ): Promise<StrongFlowStoredArtifactContent> {
    const records = await this.#artifactRecords(attemptId)
    const matches: StrongFlowStoredArtifactContent[] = []
    for (const recordValue of records) {
      if (recordValue.identity.artifactKind !== kind
        || recordValue.producer.kind !== 'role'
        || recordValue.producer.roleId !== roleId
        || recordValue.producer.stageRunId !== stageRunId
        || recordValue.producer.attemptId !== attemptId
        || (kernelSessionId !== undefined
          && recordValue.producer.eventInterval.kernelSessionId !== kernelSessionId)) continue
      const entry = await this.#readArtifactRecord(recordValue)
      if (predicate(entry)) matches.push(entry)
    }
    if (matches.length === 0) {
      handoffError('ARTIFACT_NOT_FOUND', `${kind} from the required role attempt was not found`)
    }
    if (matches.length !== 1) {
      handoffError('ARTIFACT_AMBIGUOUS', `${kind} from the required role attempt is ambiguous`)
    }
    return matches[0]!
  }

  async #artifactRecords(
    attemptId?: AttemptIdentifier,
  ): Promise<readonly StrongFlowArtifactStoreArtifactRecord[]> {
    const records: StrongFlowArtifactStoreArtifactRecord[] = []
    let afterSequence: string | undefined
    do {
      let page
      try {
        page = await this.artifactStore.list({
          limit: 1_000,
          entryKinds: ['artifact'],
          ...(afterSequence === undefined ? {} : { afterSequence }),
          ...(attemptId === undefined ? {} : { attemptId }),
        })
      } catch (error) {
        this.#storeFailure('artifact records could not be listed', error)
      }
      records.push(...page.records as readonly StrongFlowArtifactStoreArtifactRecord[])
      if (records.length > MAX_ARTIFACT_RECORD_SCAN) {
        handoffError('CONTEXT_LIMIT_EXCEEDED', 'artifact selection exceeded its bounded record scan')
      }
      afterSequence = page.nextAfterSequence ?? undefined
    } while (afterSequence !== undefined)
    return Object.freeze(records)
  }

  async #existingRoleHandoff(
    request: BuildStrongFlowRoleHandoffRequest,
  ): Promise<StrongFlowArtifactStoreHandoffRecord | undefined> {
    const records: StrongFlowArtifactStoreHandoffRecord[] = []
    let afterSequence: string | undefined
    do {
      let page
      try {
        page = await this.artifactStore.list({
          limit: 1_000,
          entryKinds: ['handoff'],
          ...(afterSequence === undefined ? {} : { afterSequence }),
        })
      } catch (error) {
        this.#storeFailure('stored handoffs could not be listed', error)
      }
      records.push(...page.records as readonly StrongFlowArtifactStoreHandoffRecord[])
      if (records.length > MAX_ARTIFACT_RECORD_SCAN) {
        handoffError('CONTEXT_LIMIT_EXCEEDED', 'handoff lookup exceeded its bounded record scan')
      }
      afterSequence = page.nextAfterSequence ?? undefined
    } while (afterSequence !== undefined)
    const matches = records.filter(recordValue => (
      recordValue.identity.target.kind === 'role'
      && recordValue.identity.target.roleId === request.roleId
      && recordValue.identity.target.stageRunId === request.stageRunId
      && recordValue.identity.target.attemptId === request.attemptId
    ))
    if (matches.length > 1) {
      handoffError('HANDOFF_CORRUPT', 'one role attempt has more than one stored handoff')
    }
    return matches[0]
  }

  async #loadReferences(
    references: readonly StrongFlowArtifactReference[],
  ): Promise<readonly StrongFlowStoredArtifactContent[]> {
    return Object.freeze(await Promise.all(references.map(
      reference => this.#loadArtifact(reference.artifactKind, reference.artifactId),
    )))
  }

  async #loadInputReferences(
    references: readonly StrongFlowHandoffInputReference[],
  ): Promise<readonly StrongFlowStoredArtifactContent[]> {
    const result: StrongFlowStoredArtifactContent[] = []
    for (const reference of references) {
      let read
      try {
        read = await this.artifactStore.read(
          reference.artifactRecordId as StrongFlowArtifactStoreArtifactRecord['recordId'],
        )
      } catch (error) {
        this.#storeFailure('pinned handoff artifact could not be read', error)
      }
      if (!('artifact' in read)
        || read.record.entryKind !== 'artifact'
        || read.artifact.artifactKind !== reference.artifactKind
        || read.artifact.artifactId !== reference.artifactId
        || read.record.blob.blobId !== reference.blobId
        || read.record.blob.byteLength !== reference.byteLength) {
        handoffError('HANDOFF_CORRUPT', 'pinned handoff input no longer matches its record')
      }
      result.push(read)
    }
    return Object.freeze(result)
  }

  async #loadArtifact(
    kind: StrongFlowArtifactKind,
    artifactId: string,
  ): Promise<StrongFlowStoredArtifactContent> {
    let result
    try {
      result = await this.artifactStore.findArtifact(kind, artifactId)
    } catch (error) {
      this.#storeFailure(`${kind} artifact could not be read`, error)
    }
    if (result === undefined) handoffError('ARTIFACT_NOT_FOUND', `${kind} artifact was not found`)
    return result
  }

  async #readArtifactRecord(
    recordValue: StrongFlowArtifactStoreArtifactRecord,
  ): Promise<StrongFlowStoredArtifactContent> {
    let result
    try {
      result = await this.artifactStore.read(recordValue.recordId)
    } catch (error) {
      this.#storeFailure('artifact record could not be read', error)
    }
    if (!('artifact' in result) || result.record.entryKind !== 'artifact') {
      handoffError('ARTIFACT_CHAIN_MISMATCH', 'artifact record changed kind')
    }
    return result
  }

  #validateSelectedKinds(
    target: StrongFlowHandoffManifest['target'],
    artifacts: readonly StrongFlowStoredArtifactContent[],
  ): void {
    const expected = target.kind === 'human-review'
      ? [
        'REQUIREMENT_SPEC',
        'SOLUTION_DESIGN',
        'SYSTEM_ARCHITECTURE_DIAGRAM',
        'PROCESS_FLOW_DIAGRAM',
      ]
      : strongFlowRoleAcceptedInputArtifacts(target.roleId)
    if (artifacts.length !== expected.length
      || expected.some((kind, index) => artifacts[index]?.artifact.artifactKind !== kind)) {
      handoffError('HANDOFF_CORRUPT', 'stored handoff input order no longer matches its target')
    }
  }

  async #publish(
    snapshot: StrongFlowJobSnapshot,
    target: StrongFlowHandoffManifest['target'],
    definition: DefinitionIdentity | null,
    candidate: StrongFlowCandidateIdentity | null,
    artifacts: readonly StrongFlowStoredArtifactContent[],
  ): Promise<StrongFlowBuiltHandoff> {
    this.#validateSelectedKinds(target, artifacts)
    const inputs = Object.freeze(artifacts.map(handoffInputReference))
    const modelInputBytes = inputs.reduce((total, input) => total + input.byteLength, 0)
    if (modelInputBytes > this.contextLimitBytes) {
      handoffError('CONTEXT_LIMIT_EXCEEDED', 'selected handoff artifacts exceed the context limit')
    }
    const base = Object.freeze({
      jobId: snapshot.jobId,
      jobSequence: snapshot.sequence,
      target,
      definition,
      candidate,
      inputs,
      modelInputBytes,
      contextLimitBytes: this.contextLimitBytes,
      producer: Object.freeze({ kind: 'system' as const, actorId: this.systemActorId }),
      createdAtMillis: snapshot.lastOccurredAtMillis,
    })
    const handoff = materializeStrongFlowHandoffManifest({
      handoffId: this.#handoffId(base),
      ...base,
    })
    let receipt
    try {
      receipt = await this.artifactStore.publishHandoff(handoff)
    } catch (error) {
      this.#storeFailure('handoff could not be published', error)
    }
    if (receipt.record.entryKind !== 'handoff') {
      handoffError('HANDOFF_CORRUPT', 'published handoff record has the wrong kind')
    }
    return Object.freeze({
      handoff,
      record: receipt.record,
      inputs: Object.freeze(artifacts.map(toRoleInput)),
    })
  }

  #handoffId(value: object): StrongFlowHandoffId {
    return `handoff-sha256-${handoffDigest(value)}` as StrongFlowHandoffId
  }

  #storeFailure(message: string, error: unknown): never {
    if (error instanceof StrongFlowHandoffError) throw error
    const known = error instanceof StrongFlowArtifactStoreError
      || error instanceof StrongFlowJobStoreError
    handoffError(known ? 'STORE_FAILURE' : 'HANDOFF_CORRUPT', message, { cause: error })
  }
}
