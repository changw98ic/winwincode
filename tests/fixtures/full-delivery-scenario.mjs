import assert from 'node:assert/strict'
import { readdir } from 'node:fs/promises'

import {
  parseStrongFlowPlanReviewContextText,
} from '../../packages/contracts/dist/index.js'
import {
  DELIVERY_MEASURES_SCHEMA_VERSION,
  createDeliveryMeasuresProjection,
  createStrongFlowPlanReviewDecision,
} from '../../packages/strongflow/dist/index.js'

import {
  DELIVERY_FIXTURE_UI_PROOF,
  DeliveryServiceFixtureTestkit,
  ScriptedDshFixtureRuntime,
  keylessFixtureEnvironment,
} from './delivery-service-testkit.mjs'

const credentialPattern = /(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/iu
const credentialNames = Object.keys(process.env).filter(name => (
  credentialPattern.test(name)
  && process.env[name] !== undefined
  && process.env[name] !== ''
))
assert.deepEqual(credentialNames, [])

const kit = await DeliveryServiceFixtureTestkit.create({
  deliveryId: 'delivery-full-keyless-rework',
})
let dshRuntime

try {
  dshRuntime = await ScriptedDshFixtureRuntime.create({
    owner: kit,
    home: kit.home,
    workspace: kit.repository,
    script: [{
      text: 'The first solution is ready for a separate human review.',
      usage: { inputTokens: 18, outputTokens: 11 },
    }, {
      text: 'The requested solution correction is ready for a new review set.',
      usage: { inputTokens: 21, outputTokens: 13 },
    }],
  })

  const firstReview = await kit.preparePlanReview({ dshRuntime })
  assert.equal(firstReview.delivery.status, 'needs-attention')
  assert.equal(firstReview.delivery.stageRuns.some(run => (
    run.stage === 'executing' || run.stage === 'reworking'
  )), false)

  const firstReviewContext = parseStrongFlowPlanReviewContextText(
    firstReview.attention.context,
  )
  const changeDecision = createStrongFlowPlanReviewDecision({
    context: firstReviewContext,
    action: 'request_changes',
    comments: 'Correct the solution before any code execution starts.',
    requestedChanges: [
      'Make the candidate failure observable before the bounded correction.',
    ],
  })
  const requestedChange = await kit.approvePlan(firstReview, {
    requestId: 'scenario:request-plan-change',
    decision: changeDecision,
  })
  assert.equal(requestedChange.ok, true)
  assert.equal(requestedChange.result.delivery.status, 'planning')
  assert.equal(requestedChange.result.delivery.stageRuns.some(run => (
    run.stage === 'executing' || run.stage === 'reworking'
  )), false)

  const revisedReview = await kit.preparePlanRevision(
    requestedChange.result.delivery,
    { prefix: 'revised', dshRuntime },
  )
  const revisedReviewContext = parseStrongFlowPlanReviewContextText(
    revisedReview.attention.context,
  )
  assert.notEqual(
    revisedReviewContext.reviewSetSha256,
    firstReviewContext.reviewSetSha256,
  )
  assert.equal(revisedReview.delivery.stageRuns.some(run => (
    run.stage === 'executing' || run.stage === 'reworking'
  )), false)

  const supersededApproval = await kit.approvePlan(revisedReview, {
    requestId: 'scenario:reject-superseded-plan-approval',
    decision: firstReview.decision,
  })
  assert.equal(supersededApproval.ok, false)
  assert.equal(supersededApproval.error.code, 'DELIVERY_CONFLICT')

  const approvedPlan = await kit.approvePlan(revisedReview, {
    requestId: 'scenario:approve-revised-plan',
  })
  assert.equal(approvedPlan.ok, true)
  assert.equal(approvedPlan.result.delivery.status, 'executing')

  const initialCandidate = await kit.prepareCandidateVerification(
    approvedPlan.result.delivery,
    {
      prefix: 'initial-defect',
      writerStage: 'executing',
      value: 'defect',
      expectedTestPass: false,
      message: 'Create observable fixture defect',
      commitDate: '2025-01-02T00:00:00Z',
    },
  )
  assert.equal(initialCandidate.repositoryCandidate.testPassed, false)
  assert.equal(initialCandidate.executingProjection.diagramExecution.state, 'executing')
  assert.equal(initialCandidate.executingProjection.diagramExecution.details, null)
  assert.equal(
    initialCandidate.finishedProjection.diagramExecution.details.candidate.candidateRef,
    initialCandidate.candidate.candidateRef,
  )
  const failingEvents = await kit.verificationEvents(initialCandidate, {
    reviewer: 'fail',
    verifier: 'fail',
  })
  const failedVerdictResponse = await kit.submitVerdict(
    initialCandidate,
    failingEvents,
    { requestId: 'scenario:submit-failed-verdict' },
  )
  assert.equal(failedVerdictResponse.ok, true)
  const failedDelivery = failedVerdictResponse.result.delivery
  assert.equal(failedDelivery.status, 'needs-attention')
  assert.equal(failedDelivery.verdict.status, 'fail')
  assert.equal(failedDelivery.verdict.candidateRef, initialCandidate.candidate.candidateRef)
  const reworkAttention = failedDelivery.attentionItems.find(item => (
    item.status === 'open'
    && item.options.some(option => option.id === 'start-rework')
  ))
  assert.notEqual(reworkAttention, undefined)
  const reworkContext = JSON.parse(reworkAttention.context)
  assert.equal(reworkContext.verdictId, failedDelivery.verdict.id)
  assert.equal(
    reworkContext.criterionId,
    failedDelivery.spec.acceptanceCriteria[0].id,
  )

  const reworking = await kit.requireSuccess(
    'resolveAttention',
    'scenario:approve-bounded-rework',
    {
      deliveryId: kit.deliveryId,
      expectedRevision: failedDelivery.revision,
      attentionItemId: reworkAttention.id,
      status: 'resolved',
      resolution: [
        'Correct the exact failed value module.',
        `Criterion: ${reworkContext.criterionId}.`,
        `Candidate: ${initialCandidate.candidate.candidateRef}.`,
      ].join(' '),
      remediation: null,
      channel: 'local-ui',
      authentication: {
        scheme: 'local-session',
        proof: DELIVERY_FIXTURE_UI_PROOF,
      },
    },
  )
  assert.equal(reworking.status, 'reworking')

  const correctedCandidate = await kit.prepareCandidateVerification(reworking, {
    prefix: 'corrected',
    writerStage: 'reworking',
    value: 'after',
    expectedTestPass: true,
    message: 'Correct observable fixture defect',
    commitDate: '2025-01-03T00:00:00Z',
  })
  assert.equal(correctedCandidate.repositoryCandidate.testPassed, true)
  assert.notEqual(
    correctedCandidate.candidate.candidateRef,
    initialCandidate.candidate.candidateRef,
  )
  const passingEvents = await kit.verificationEvents(correctedCandidate, {
    reviewer: 'pass',
    verifier: 'pass',
  })
  const staleCandidate = await kit.submitVerdict(
    correctedCandidate,
    passingEvents,
    {
      requestId: 'scenario:reject-superseded-candidate',
      candidate: initialCandidate.candidate,
    },
  )
  assert.equal(staleCandidate.ok, false)
  assert.equal(staleCandidate.error.code, 'INVALID_REQUEST')

  const passedVerdictResponse = await kit.submitVerdict(
    correctedCandidate,
    passingEvents,
    { requestId: 'scenario:submit-corrected-verdict' },
  )
  assert.equal(passedVerdictResponse.ok, true)
  const readyToDeliver = passedVerdictResponse.result.delivery
  assert.equal(readyToDeliver.status, 'ready-to-deliver')
  assert.equal(readyToDeliver.verdict.status, 'pass')
  assert.equal(
    readyToDeliver.verdict.candidateRef,
    correctedCandidate.candidate.candidateRef,
  )
  const currentEvidenceIds = new Set(readyToDeliver.verdict.criteria.flatMap(result => (
    result.evidenceRefs
  )))
  const currentEvidence = readyToDeliver.evidence.filter(reference => (
    currentEvidenceIds.has(reference.id)
  ))
  assert.equal(currentEvidence.length, currentEvidenceIds.size)
  assert.equal(currentEvidence.every(reference => (
    reference.deliverySpecId === readyToDeliver.spec.id
    && reference.deliverySpecRevision === readyToDeliver.spec.revision
    && reference.candidateRef === correctedCandidate.candidate.candidateRef
  )), true)
  assert.equal(currentEvidence.some(reference => (
    reference.candidateRef === initialCandidate.candidate.candidateRef
  )), false)

  const deliveryReview = await kit.prepareDeliveryReview(readyToDeliver)
  const deliveredResponse = await kit.approveDelivery(deliveryReview, {
    requestId: 'scenario:approve-final-delivery',
  })
  assert.equal(deliveredResponse.ok, true)
  const delivered = deliveredResponse.result.delivery
  assert.equal(delivered.status, 'delivered')
  assert.equal(delivered.verdict.status, 'pass')
  assert.equal(delivered.verdict.candidateRef, correctedCandidate.candidate.candidateRef)

  const unboundStages = delivered.stageRuns.filter(run => {
    const binding = delivered.sessionBindings.find(entry => entry.stageRunId === run.id)
    return binding === undefined
      || binding.dshSessionId === null
      || (run.actorType === 'codex' && binding.codexSessionId === null)
  })
  assert.deepEqual(unboundStages, [])
  const runtimeProjection = await kit.runtimeProjection(delivered)
  const projectedAgents = runtimeProjection.stages.flatMap(stage => (
    stage.sessions.flatMap(session => session.agents)
  ))
  const projectedSubagents = projectedAgents.filter(agent => (
    agent.threadId.includes('codex-fixture-subagent')
  ))
  assert.equal(projectedSubagents.length >= 2, true)
  assert.equal(Object.hasOwn(delivered, 'agentGraph'), false)
  assert.equal(Object.hasOwn(delivered, 'plan'), false)

  assert.deepEqual(dshRuntime.calls.map(call => ({
    provider: call.provider,
    model: call.model,
  })), [{
    provider: 'fixture',
    model: 'fixture-coder',
  }, {
    provider: 'fixture',
    model: 'fixture-coder',
  }])
  assert.equal(dshRuntime.remainingResponses, 0)
  assert.deepEqual((await readdir(kit.root)).toSorted(), ['home', 'repository'])

  const measures = createDeliveryMeasuresProjection({
    schemaVersion: DELIVERY_MEASURES_SCHEMA_VERSION,
    runKind: 'deterministic',
    runId: 'deterministic-full-delivery',
    runState: 'completed',
    startedAtMillis: delivered.createdAtMillis,
    finishedAtMillis: delivered.updatedAtMillis,
    delivery: delivered,
    runtimeProjection,
    requiredVerificationRoles: ['reviewer', 'verifier'],
    modelCalls: dshRuntime.calls.map((call, index) => Object.freeze({
      sourceRef: `evaluation_run:deterministic-full-delivery#/modelCalls/${String(index)}`,
      status: 'completed',
      startedAtMillis: null,
      finishedAtMillis: null,
      inputTokens: call.usage.inputTokens,
      outputTokens: call.usage.outputTokens,
      cacheReadTokens: call.usage.cacheReadTokens ?? 0,
      cacheWriteTokens: call.usage.cacheWriteTokens ?? 0,
      costUsdMicros: null,
    })),
    pricingSource: null,
    historicalVerdicts: [failedDelivery.verdict, delivered.verdict],
  })
  assert.equal(measures.outcome.classification.value, 'proven-success')
  assert.equal(measures.dimensions.completeness.status.value, 'complete')
  assert.equal(measures.dimensions.confidence.status.value, 'independently-supported')
  assert.equal(measures.dimensions.stability.status.value, 'reworked')
  assert.equal(measures.dimensions.efficiency.totalTokens.value, 63)

  process.stdout.write(`${JSON.stringify({
    deliveryId: delivered.id,
    finalStatus: delivered.status,
    finalRevision: delivered.revision,
    spec: {
      id: delivered.spec.id,
      revision: delivered.spec.revision,
    },
    planReview: {
      firstDigest: firstReviewContext.reviewSetSha256,
      revisedDigest: revisedReviewContext.reviewSetSha256,
      supersededApprovalError: supersededApproval.error.code,
    },
    humanGate: {
      statusBeforeDecision: firstReview.delivery.status,
      reviewStageStatus: firstReview.delivery.stageRuns.at(-1).status,
      executionStageCountBeforeDecision: firstReview.delivery.stageRuns.filter(run => (
        run.stage === 'executing' || run.stage === 'reworking'
      )).length,
      scriptedFirstDecision: changeDecision.action,
      revisedPlanApproved: approvedPlan.result.delivery.status === 'executing',
    },
    candidates: {
      failed: initialCandidate.candidate.candidateRef,
      passed: correctedCandidate.candidate.candidateRef,
      staleCandidateError: staleCandidate.error.code,
    },
    verdicts: {
      failed: failedDelivery.verdict.status,
      passed: delivered.verdict.status,
    },
    criterionResults: delivered.verdict.criteria.map(result => ({
      id: result.id,
      criterionId: result.criterionId,
      verdict: result.verdict,
      evidenceRefs: result.evidenceRefs,
      explanation: result.explanation,
    })),
    deliveryVerdict: {
      id: delivered.verdict.id,
      status: delivered.verdict.status,
      candidateRef: delivered.verdict.candidateRef,
      unresolvedFindings: delivered.verdict.unresolvedFindings,
    },
    evidenceCount: currentEvidence.length,
    stageCount: delivered.stageRuns.length,
    bindingCount: delivered.sessionBindings.length,
    projectedSubagentCount: projectedSubagents.length,
    modelCalls: dshRuntime.calls.length,
    measures,
    releaseGateFixture: {
      delivery: delivered,
      candidate: correctedCandidate.candidate,
      runtimeProjection,
      modelCalls: dshRuntime.calls.map(call => ({ usage: call.usage })),
    },
    credentialNames,
    rootEntries: (await readdir(kit.root)).toSorted(),
  })}\n`)
} finally {
  await kit.cleanup()
}
