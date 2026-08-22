import assert from 'node:assert/strict'
import test from 'node:test'

import {
  CandidateId,
  DiagramId,
  ExecutionChangeAnnotationId,
  ExecutionPlanId,
  HumanReviewId,
  JobId,
  PatchManifestId,
  RemediationReportId,
  RemediationRequestId,
  RequirementId,
  ReviewReportId,
  SolutionId,
  StrongFlowArtifactValidationError,
  UserRequestId,
  VerificationReportId,
  DeliveryReceiptId,
  materializeStrongFlowArtifact,
  parseStrongFlowArtifact,
  parseStrongFlowArtifactAs,
  parseStrongFlowArtifactPayload,
  requireCurrentExecutionChangeAnnotation,
} from '../packages/contracts/dist/index.js'
import {
  createStrongFlowCanonicalRoleArtifactValidator,
} from '../packages/strongflow/dist/index.js'

const HASH_A = 'a'.repeat(64)
const HASH_B = 'b'.repeat(64)
const HASH_C = 'c'.repeat(64)
const HASH_D = 'd'.repeat(64)
const JOB_ID = JobId('artifact-job-1')

const ids = Object.freeze({
  user: UserRequestId('user-request-1'),
  requirement: RequirementId('requirement-1'),
  solution: SolutionId('solution-1'),
  architecture: DiagramId('architecture-1'),
  process: DiagramId('process-1'),
  review: HumanReviewId('human-review-1'),
  plan: ExecutionPlanId('execution-plan-1'),
  patch: PatchManifestId('patch-manifest-1'),
  reviewReport: ReviewReportId('review-report-1'),
  verification: VerificationReportId('verification-report-1'),
  annotation: ExecutionChangeAnnotationId('annotation-1'),
  remediationRequest: RemediationRequestId('remediation-request-1'),
  remediatedPatch: PatchManifestId('patch-manifest-2'),
  remediationReport: RemediationReportId('remediation-report-1'),
  finalVerification: VerificationReportId('verification-report-2'),
  delivery: DeliveryReceiptId('delivery-receipt-1'),
})

const definition = Object.freeze({
  requirementId: ids.requirement,
  solutionId: ids.solution,
  systemArchitectureDiagramId: ids.architecture,
  processFlowDiagramId: ids.process,
})

const candidate = Object.freeze({
  candidateId: CandidateId('candidate-1'),
  sourceSnapshotId: `source-sha256-${HASH_A}`,
  baseCommitId: '1'.repeat(40),
  baseTreeId: '2'.repeat(40),
  candidateCommitId: '3'.repeat(40),
  candidateTreeId: '4'.repeat(40),
  diffId: HASH_B,
})

const remediatedCandidate = Object.freeze({
  ...candidate,
  candidateId: CandidateId('candidate-2'),
  candidateCommitId: '5'.repeat(40),
  candidateTreeId: '6'.repeat(40),
  diffId: HASH_C,
})

const definitionSources = Object.freeze([
  Object.freeze({ artifactKind: 'REQUIREMENT_SPEC', artifactId: ids.requirement }),
  Object.freeze({ artifactKind: 'SOLUTION_DESIGN', artifactId: ids.solution }),
  Object.freeze({
    artifactKind: 'SYSTEM_ARCHITECTURE_DIAGRAM',
    artifactId: ids.architecture,
  }),
  Object.freeze({ artifactKind: 'PROCESS_FLOW_DIAGRAM', artifactId: ids.process }),
])

const approvedDefinitionSources = Object.freeze([
  ...definitionSources,
  Object.freeze({ artifactKind: 'HUMAN_REVIEW_RECORD', artifactId: ids.review }),
])

function ref(artifactKind, artifactId) {
  return Object.freeze({ artifactKind, artifactId })
}

function roleProducer(roleId, suffix = roleId) {
  return Object.freeze({
    kind: 'role',
    roleId,
    stageRunId: `stage-${suffix}`,
    attemptId: `attempt-${suffix}`,
  })
}

function interval(suffix = '1') {
  return Object.freeze({
    schemaVersion: 1,
    kernelSessionLineageId: `lineage-${suffix}`,
    contextId: `context-${suffix}`,
    generation: 1,
    kernelSessionId: `kernel-${suffix}`,
    kernelStreamId: `stream-${suffix}`,
    turnId: `turn-${suffix}`,
    firstSequence: '10',
    lastSequence: '12',
    eventCount: 3,
  })
}

function roleMetadata(artifactId, sourceArtifacts, roleId, suffix = roleId) {
  return Object.freeze({
    artifactId,
    jobId: JOB_ID,
    sourceArtifacts,
    producer: roleProducer(roleId, suffix),
    kernelEventInterval: interval(suffix),
    createdAtMillis: 1_900_000_000_000,
  })
}

function humanMetadata(artifactId, sourceArtifacts, actorId = 'reviewer-1') {
  return Object.freeze({
    artifactId,
    jobId: JOB_ID,
    sourceArtifacts,
    producer: Object.freeze({ kind: 'human', actorId, channel: 'local-ui' }),
    kernelEventInterval: null,
    createdAtMillis: 1_900_000_000_001,
  })
}

function systemMetadata(artifactId, sourceArtifacts) {
  return Object.freeze({
    artifactId,
    jobId: JOB_ID,
    sourceArtifacts,
    producer: Object.freeze({ kind: 'system', actorId: 'strongflow-controller' }),
    kernelEventInterval: null,
    createdAtMillis: 1_900_000_000_002,
  })
}

const requirementPayload = Object.freeze({
  title: 'Show execution changes on both review diagrams',
  summary: 'Keep stable diagram nodes and attach completed changes to them.',
  goals: [{ id: 'goal-1', text: 'Show which diagram nodes changed.' }],
  nonGoals: [{ id: 'non-goal-1', text: 'Do not expose exact diffs while running.' }],
  constraints: [{ id: 'constraint-1', text: 'Use the current candidate identity.' }],
  acceptanceCriteria: [{
    criterionId: 'criterion-1',
    statement: 'Changed nodes are clickable only after execution.',
    verification: 'Exercise before, during, and finished states.',
  }],
  repositoryFacts: [{
    factId: 'fact-1',
    statement: 'StrongFlow owns the review workflow.',
    evidence: 'packages/strongflow/src',
  }],
  risks: [{
    riskId: 'risk-1',
    statement: 'A stale node could receive a review comment.',
    mitigation: 'Validate candidate, diff, diagram, node, and hunk identities.',
  }],
  openQuestions: [],
})

const solutionPayload = Object.freeze({
  requirementId: ids.requirement,
  summary: 'Use immutable artifact identities and stable diagram nodes.',
  decisions: [{
    decisionId: 'decision-1',
    title: 'Canonical artifact parser',
    decision: 'All boundaries call one exact parser.',
    rationale: 'Different parsers could accept different security fields.',
    requirementItemIds: ['goal-1', 'criterion-1'],
  }],
  components: [{
    componentId: 'component-contracts',
    name: 'Artifact contracts',
    kind: 'module',
    responsibility: 'Validate role and review artifacts.',
    trustBoundary: 'local-process',
    sourcePaths: ['packages/contracts/src/strongflow-artifact.ts'],
  }],
  connections: [],
  unresolvedFacts: [],
  risks: [],
})

const architecturePayload = Object.freeze({
  requirementId: ids.requirement,
  solutionId: ids.solution,
  title: 'StrongFlow artifact architecture',
  nodes: [{
    nodeId: 'node-contracts',
    label: 'Artifact contracts',
    kind: 'module',
    description: 'Owns the canonical schemas.',
    trustBoundary: 'local-process',
    unresolved: false,
    componentIds: ['component-contracts'],
    sourcePaths: ['packages/contracts/src/strongflow-artifact.ts'],
  }],
  edges: [],
})

const processPayload = Object.freeze({
  requirementId: ids.requirement,
  solutionId: ids.solution,
  title: 'StrongFlow review process',
  nodes: [
    {
      nodeId: 'node-before',
      label: 'Before execution',
      kind: 'start',
      description: 'All nodes are green.',
      roleId: null,
      unresolved: false,
    },
    {
      nodeId: 'node-executor',
      label: 'Execute',
      kind: 'stage',
      description: 'Changed nodes become light blue.',
      roleId: 'executor',
      unresolved: false,
    },
    {
      nodeId: 'node-review',
      label: 'Review',
      kind: 'human-review',
      description: 'Changed nodes become yellow and clickable.',
      roleId: null,
      unresolved: false,
    },
  ],
  edges: [
    { edgeId: 'edge-1', fromNodeId: 'node-before', toNodeId: 'node-executor', label: '' },
    { edgeId: 'edge-2', fromNodeId: 'node-executor', toNodeId: 'node-review', label: '' },
  ],
})

function examples() {
  const user = materializeStrongFlowArtifact('USER_REQUEST', humanMetadata(ids.user, []), {
    request: 'Add reviewable execution changes to both diagrams.',
    submittedFrom: 'strongflow-workbench',
  })
  const requirement = materializeStrongFlowArtifact(
    'REQUIREMENT_SPEC',
    roleMetadata(ids.requirement, [ref('USER_REQUEST', ids.user)], 'requirements'),
    requirementPayload,
  )
  const solution = materializeStrongFlowArtifact(
    'SOLUTION_DESIGN',
    roleMetadata(ids.solution, [ref('REQUIREMENT_SPEC', ids.requirement)], 'solution'),
    solutionPayload,
  )
  const architecture = materializeStrongFlowArtifact(
    'SYSTEM_ARCHITECTURE_DIAGRAM',
    roleMetadata(ids.architecture, [
      ref('REQUIREMENT_SPEC', ids.requirement),
      ref('SOLUTION_DESIGN', ids.solution),
    ], 'solution', 'solution-architecture'),
    architecturePayload,
  )
  const process = materializeStrongFlowArtifact(
    'PROCESS_FLOW_DIAGRAM',
    roleMetadata(ids.process, [
      ref('REQUIREMENT_SPEC', ids.requirement),
      ref('SOLUTION_DESIGN', ids.solution),
    ], 'solution', 'solution-process'),
    processPayload,
  )
  const review = materializeStrongFlowArtifact(
    'HUMAN_REVIEW_RECORD',
    humanMetadata(ids.review, definitionSources),
    { definition, decision: 'approved', comment: 'Proceed.', scope: null },
  )
  const plan = materializeStrongFlowArtifact(
    'EXECUTION_PLAN',
    roleMetadata(ids.plan, approvedDefinitionSources, 'planner'),
    {
      definition,
      humanReviewId: ids.review,
      summary: 'Implement and validate the canonical artifacts.',
      steps: [{
        stepId: 'step-contracts',
        title: 'Add artifact contracts',
        instructions: 'Add exact schemas and their tests.',
        dependsOn: [],
        paths: ['packages/contracts/src/strongflow-artifact.ts'],
        commands: ['corepack pnpm typecheck'],
        checks: ['All examples round-trip.'],
      }],
    },
  )
  const executorSources = [...approvedDefinitionSources, ref('EXECUTION_PLAN', ids.plan)]
  const patch = materializeStrongFlowArtifact(
    'PATCH_MANIFEST',
    roleMetadata(ids.patch, executorSources, 'executor'),
    {
      executionPlanId: ids.plan,
      candidate,
      remediationRequestId: null,
      changedFiles: [{
        path: 'packages/contracts/src/strongflow-artifact.ts',
        changeType: 'added',
        previousPath: null,
        hunks: [{
          hunkId: 'hunk-contracts-1',
          oldStart: 0,
          oldLines: 0,
          newStart: 1,
          newLines: 20,
          summary: 'Add artifact schemas.',
          diagramNodeIds: ['node-contracts'],
        }],
      }],
      commands: [{
        evidenceId: 'evidence-typecheck',
        command: 'corepack pnpm typecheck',
        exitCode: 0,
        summary: 'TypeScript accepted the contracts.',
        outputSha256: HASH_D,
      }],
      tests: [],
    },
  )
  const candidateSources = [...approvedDefinitionSources, ref('EXECUTION_PLAN', ids.plan), ref('PATCH_MANIFEST', ids.patch)]
  const reviewReport = materializeStrongFlowArtifact(
    'REVIEW_REPORT',
    roleMetadata(ids.reviewReport, candidateSources, 'reviewer'),
    {
      patchManifestId: ids.patch,
      candidate,
      outcome: 'changes-required',
      summary: 'One current-node check is required.',
      findings: [{
        findingId: 'finding-stale-node',
        severity: 'major',
        title: 'Reject stale node annotations',
        message: 'The annotation target must match the current diagram.',
        location: {
          path: 'packages/contracts/src/strongflow-artifact.ts',
          hunkId: 'hunk-contracts-1',
        },
        diagramNodeIds: ['node-contracts'],
        disposition: 'open',
      }],
    },
  )
  const verification = materializeStrongFlowArtifact(
    'VERIFICATION_REPORT',
    roleMetadata(ids.verification, [
      ...candidateSources,
      ref('REVIEW_REPORT', ids.reviewReport),
    ], 'verifier'),
    {
      patchManifestId: ids.patch,
      candidate,
      mode: 'standard',
      outcome: 'failed',
      summary: 'The stale-node negative test is pending.',
      checks: [{
        checkId: 'check-round-trip',
        title: 'Artifact round-trip',
        command: 'node --test tests/strongflow-artifact.test.mjs',
        outcome: 'passed',
        evidence: 'Every example survived JSON serialization and parsing.',
        relatedFindingIds: [],
      }],
    },
  )
  const annotation = materializeStrongFlowArtifact(
    'EXECUTION_CHANGE_ANNOTATION',
    humanMetadata(ids.annotation, [
      ref('SYSTEM_ARCHITECTURE_DIAGRAM', ids.architecture),
      ref('PATCH_MANIFEST', ids.patch),
    ], 'reviewer-annotation'),
    {
      candidateId: candidate.candidateId,
      diffId: candidate.diffId,
      patchManifestId: ids.patch,
      diagramId: ids.architecture,
      diagramKind: 'SYSTEM_ARCHITECTURE_DIAGRAM',
      nodeId: 'node-contracts',
      location: {
        path: 'packages/contracts/src/strongflow-artifact.ts',
        hunkId: 'hunk-contracts-1',
      },
      message: 'Add the stale-node negative case.',
      disposition: 'open',
    },
  )
  const remediationRequestSources = [
    ...candidateSources,
    ref('REVIEW_REPORT', ids.reviewReport),
    ref('VERIFICATION_REPORT', ids.verification),
    ref('EXECUTION_CHANGE_ANNOTATION', ids.annotation),
  ]
  const remediationRequest = materializeStrongFlowArtifact(
    'REMEDIATION_REQUEST',
    systemMetadata(ids.remediationRequest, remediationRequestSources),
    {
      candidate,
      patchManifestId: ids.patch,
      reason: 'Resolve the reviewer finding and annotation.',
      findings: [{
        sourceArtifactKind: 'REVIEW_REPORT',
        sourceArtifactId: ids.reviewReport,
        findingId: 'finding-stale-node',
        instruction: 'Reject stale diagram nodes before accepting the annotation.',
        diagramNodeIds: ['node-contracts'],
      }],
      annotationIds: [ids.annotation],
      boundedPaths: ['packages/contracts/src/strongflow-artifact.ts'],
    },
  )
  const remediatorSources = [
    ...candidateSources,
    ref('REVIEW_REPORT', ids.reviewReport),
    ref('VERIFICATION_REPORT', ids.verification),
    ref('REMEDIATION_REQUEST', ids.remediationRequest),
  ]
  const remediatedPatch = materializeStrongFlowArtifact(
    'PATCH_MANIFEST',
    roleMetadata(ids.remediatedPatch, remediatorSources, 'remediator', 'remediator-patch'),
    {
      executionPlanId: ids.plan,
      candidate: remediatedCandidate,
      remediationRequestId: ids.remediationRequest,
      changedFiles: [{
        path: 'packages/contracts/src/strongflow-artifact.ts',
        changeType: 'modified',
        previousPath: null,
        hunks: [{
          hunkId: 'hunk-contracts-2',
          oldStart: 20,
          oldLines: 2,
          newStart: 20,
          newLines: 8,
          summary: 'Reject stale annotation targets.',
          diagramNodeIds: ['node-contracts'],
        }],
      }],
      commands: [],
      tests: [{
        evidenceId: 'evidence-stale-node',
        command: 'node --test tests/strongflow-artifact.test.mjs',
        exitCode: 0,
        summary: 'Stale node and hunk targets were rejected.',
        outputSha256: HASH_A,
      }],
    },
  )
  const remediationReport = materializeStrongFlowArtifact(
    'REMEDIATION_REPORT',
    roleMetadata(ids.remediationReport, [
      ...remediatorSources,
      ref('PATCH_MANIFEST', ids.remediatedPatch),
    ], 'remediator', 'remediator-report'),
    {
      remediationRequestId: ids.remediationRequest,
      patchManifestId: ids.remediatedPatch,
      candidate: remediatedCandidate,
      summary: 'The stale target check is now enforced.',
      addressedFindingIds: ['finding-stale-node'],
      addressedAnnotationIds: [ids.annotation],
      residualRisks: [],
    },
  )
  const finalCandidateSources = [
    ...approvedDefinitionSources,
    ref('EXECUTION_PLAN', ids.plan),
    ref('PATCH_MANIFEST', ids.remediatedPatch),
  ]
  const finalVerification = materializeStrongFlowArtifact(
    'VERIFICATION_REPORT',
    roleMetadata(ids.finalVerification, [
      ...finalCandidateSources,
      ref('REVIEW_REPORT', ids.reviewReport),
    ], 'verifier', 'final-verifier'),
    {
      patchManifestId: ids.remediatedPatch,
      candidate: remediatedCandidate,
      mode: 'standard',
      outcome: 'passed',
      summary: 'All current-target checks pass.',
      checks: [{
        checkId: 'check-stale-target',
        title: 'Reject stale targets',
        command: 'node --test tests/strongflow-artifact.test.mjs',
        outcome: 'passed',
        evidence: 'Candidate, diff, node, and hunk mismatches were rejected.',
        relatedFindingIds: ['finding-stale-node'],
      }],
    },
  )
  const delivery = materializeStrongFlowArtifact(
    'DELIVERY_RECEIPT',
    systemMetadata(ids.delivery, [
      ...finalCandidateSources,
      ref('REVIEW_REPORT', ids.reviewReport),
      ref('VERIFICATION_REPORT', ids.finalVerification),
      ref('REMEDIATION_REPORT', ids.remediationReport),
    ]),
    {
      definition,
      humanReviewId: ids.review,
      executionPlanId: ids.plan,
      patchManifestId: ids.remediatedPatch,
      candidate: remediatedCandidate,
      reviewReportId: ids.reviewReport,
      verificationReportIds: [ids.finalVerification],
      remediationReportId: ids.remediationReport,
      summary: 'The exact approved candidate is ready for delivery.',
    },
  )
  return Object.freeze([
    user,
    requirement,
    solution,
    architecture,
    process,
    review,
    plan,
    patch,
    reviewReport,
    verification,
    annotation,
    remediationRequest,
    remediatedPatch,
    remediationReport,
    finalVerification,
    delivery,
  ])
}

function artifactError(code) {
  return error => error instanceof StrongFlowArtifactValidationError && error.code === code
}

test('all canonical artifact examples round-trip through the one boundary parser', () => {
  const artifacts = examples()
  const kinds = new Set(artifacts.map(artifact => artifact.artifactKind))
  assert.deepEqual(kinds, new Set([
    'USER_REQUEST',
    'REQUIREMENT_SPEC',
    'SOLUTION_DESIGN',
    'SYSTEM_ARCHITECTURE_DIAGRAM',
    'PROCESS_FLOW_DIAGRAM',
    'HUMAN_REVIEW_RECORD',
    'EXECUTION_PLAN',
    'PATCH_MANIFEST',
    'REVIEW_REPORT',
    'VERIFICATION_REPORT',
    'REMEDIATION_REQUEST',
    'REMEDIATION_REPORT',
    'DELIVERY_RECEIPT',
    'EXECUTION_CHANGE_ANNOTATION',
  ]))
  for (const artifact of artifacts) {
    const transported = JSON.parse(JSON.stringify(artifact))
    for (const boundary of ['native', 'disk', 'ui', 'command']) {
      const parsed = parseStrongFlowArtifact(structuredClone(transported))
      assert.deepEqual(parsed, artifact, `${artifact.artifactKind} failed at ${boundary}`)
      assert.ok(Object.isFrozen(parsed))
      assert.ok(Object.isFrozen(parsed.payload))
      assert.ok(Object.isFrozen(parsed.sourceArtifacts))
    }
  }
})

test('requirement and definition boundaries reject hidden solution fields and stale identities', () => {
  assert.throws(
    () => parseStrongFlowArtifactPayload('REQUIREMENT_SPEC', {
      ...requirementPayload,
      solutionDesign: { components: ['hidden'] },
    }),
    artifactError('INVALID_ARTIFACT_SHAPE'),
  )

  const requirement = examples().find(artifact => artifact.artifactKind === 'REQUIREMENT_SPEC')
  assert.ok(requirement)
  assert.throws(
    () => parseStrongFlowArtifact({ ...requirement, schemaVersion: 2 }),
    artifactError('UNSUPPORTED_SCHEMA_VERSION'),
  )
  assert.throws(
    () => parseStrongFlowArtifact({ ...requirement, authorization: 'skip-review' }),
    artifactError('INVALID_ARTIFACT_SHAPE'),
  )
  assert.throws(
    () => materializeStrongFlowArtifact(
      'SOLUTION_DESIGN',
      roleMetadata(ids.solution, [ref('REQUIREMENT_SPEC', 'requirement-stale')], 'solution'),
      solutionPayload,
    ),
    artifactError('INVALID_RELATIONSHIP'),
  )

  assert.throws(
    () => materializeStrongFlowArtifact(
      'HUMAN_REVIEW_RECORD',
      humanMetadata(ids.review, definitionSources.slice(0, 3)),
      { definition, decision: 'approved', comment: null, scope: null },
    ),
    artifactError('INVALID_SOURCE_ARTIFACTS'),
  )
})

test('the role-runner adapter adds trusted identity and provenance after model validation', () => {
  const validator = createStrongFlowCanonicalRoleArtifactValidator({
    kind: 'REQUIREMENT_SPEC',
    artifactId: ids.requirement,
    createdAtMillis: 1_900_000_000_100,
  })
  const context = Object.freeze({
    roleSession: Object.freeze({
      schemaVersion: 1,
      kernelSessionLineageId: 'lineage-model-output',
      contextId: 'context-model-output',
      roleSpecId: 'role-spec-model-output',
      jobId: JOB_ID,
      stageRunId: 'stage-requirements',
      attemptId: 'attempt-requirements',
      roleSpec: Object.freeze({
        id: 'requirements',
        acceptedInputArtifacts: Object.freeze(['USER_REQUEST']),
      }),
    }),
    artifactKind: 'REQUIREMENT_SPEC',
    inputArtifactIds: Object.freeze([ids.user]),
    eventInterval: Object.freeze({
      schemaVersion: 1,
      contextId: 'context-model-output',
      generation: 1,
      kernelSessionId: 'kernel-model-output',
      kernelStreamId: 'stream-model-output',
      turnId: 'turn-model-output',
      firstSequence: '1',
      lastSequence: '3',
      eventCount: 3,
    }),
    usage: Object.freeze({}),
  })
  const artifact = validator.validate(requirementPayload, context)
  assert.equal(artifact.artifactId, ids.requirement)
  assert.equal(artifact.producer.roleId, 'requirements')
  assert.deepEqual(artifact.sourceArtifacts, [ref('USER_REQUEST', ids.user)])
  assert.equal(artifact.kernelEventInterval.kernelSessionLineageId, 'lineage-model-output')

  assert.throws(
    () => validator.validate({ ...requirementPayload, artifactId: 'model-chosen-id' }, context),
    artifactError('INVALID_ARTIFACT_SHAPE'),
  )
  assert.throws(
    () => validator.validate(requirementPayload, {
      ...context,
      inputArtifactIds: [],
    }),
    artifactError('SOURCE_INPUT_MISMATCH'),
  )

  const examplePatch = examples().find(artifact => artifact.artifactKind === 'PATCH_MANIFEST')
  assert.ok(examplePatch)
  const executorValidator = createStrongFlowCanonicalRoleArtifactValidator({
    kind: 'PATCH_MANIFEST',
    artifactId: PatchManifestId('patch-from-plan-only-handoff'),
    createdAtMillis: 1_900_000_000_200,
    sourceArtifacts: examplePatch.sourceArtifacts,
  })
  const executorContext = Object.freeze({
    ...context,
    roleSession: Object.freeze({
      ...context.roleSession,
      stageRunId: 'stage-executor',
      attemptId: 'attempt-executor',
      roleSpec: Object.freeze({
        id: 'executor',
        acceptedInputArtifacts: Object.freeze(['EXECUTION_PLAN']),
      }),
    }),
    artifactKind: 'PATCH_MANIFEST',
    inputArtifactIds: Object.freeze([ids.plan]),
  })
  const planOnlyPatch = executorValidator.validate(examplePatch.payload, executorContext)
  assert.equal(planOnlyPatch.payload.executionPlanId, ids.plan)
  assert.deepEqual(planOnlyPatch.sourceArtifacts, examplePatch.sourceArtifacts)

  const missingPlanSource = createStrongFlowCanonicalRoleArtifactValidator({
    kind: 'PATCH_MANIFEST',
    artifactId: PatchManifestId('patch-missing-visible-plan-source'),
    createdAtMillis: 1_900_000_000_201,
    sourceArtifacts: examplePatch.sourceArtifacts.slice(0, -1),
  })
  assert.throws(
    () => missingPlanSource.validate(examplePatch.payload, executorContext),
    artifactError('SOURCE_INPUT_MISMATCH'),
  )
})

test('execution annotations accept only the current candidate, node, and exact hunk', () => {
  const annotation = examples().find(
    artifact => artifact.artifactKind === 'EXECUTION_CHANGE_ANNOTATION',
  )
  assert.ok(annotation)
  const currentTarget = Object.freeze({
    candidate,
    patchManifestId: ids.patch,
    diagramKind: 'SYSTEM_ARCHITECTURE_DIAGRAM',
    diagramId: ids.architecture,
    nodeIds: ['node-contracts'],
    hunks: [{
      path: 'packages/contracts/src/strongflow-artifact.ts',
      hunkId: 'hunk-contracts-1',
    }],
  })
  assert.deepEqual(requireCurrentExecutionChangeAnnotation(annotation, currentTarget), annotation)

  for (const staleTarget of [
    { ...currentTarget, candidate: { ...candidate, candidateId: 'candidate-stale' } },
    { ...currentTarget, candidate: { ...candidate, diffId: HASH_A } },
    { ...currentTarget, nodeIds: ['node-removed'] },
    { ...currentTarget, hunks: [{
      path: 'packages/contracts/src/strongflow-artifact.ts',
      hunkId: 'hunk-replaced',
    }] },
  ]) {
    assert.throws(
      () => requireCurrentExecutionChangeAnnotation(annotation, staleTarget),
      artifactError('STALE_ANNOTATION_TARGET'),
    )
  }

  assert.throws(
    () => parseStrongFlowArtifactAs('PROCESS_FLOW_DIAGRAM', annotation),
    artifactError('ARTIFACT_KIND_MISMATCH'),
  )
})
