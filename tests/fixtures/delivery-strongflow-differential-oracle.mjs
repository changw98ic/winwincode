import { createHash } from 'node:crypto'
import { resolve } from 'node:path'

const CROCKFORD_BASE32 = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'

function oracleDeliveryId(label) {
  const bytes = createHash('sha256')
    .update(`winwincode.delivery-oracle-id.v1\0${label}`)
    .digest()
    .subarray(0, 16)
  let value = BigInt(`0x${bytes.toString('hex')}`)
  const suffix = Array(26)
  for (let index = suffix.length - 1; index >= 0; index -= 1) {
    suffix[index] = CROCKFORD_BASE32[Number(value & 31n)]
    value >>= 5n
  }
  return `dlv_${suffix.join('')}`
}

function replaceEvery(value, from, to) {
  return from.length === 0 ? value : value.split(from).join(to)
}

function normalizeString(value, options) {
  let normalized = value
  const root = resolve(options.root)
  normalized = replaceEvery(normalized, root, '<ORACLE_ROOT>')
  normalized = replaceEvery(normalized, options.nodeExecutable ?? process.execPath, '<NODE_EXECUTABLE>')
  for (const [source, replacement] of Object.entries(options.randomIds ?? {})) {
    normalized = replaceEvery(normalized, source, replacement)
  }
  return normalized
}

/**
 * Canonicalizes host-only fixture facts while preserving every product fact.
 * Object keys are sorted so the committed JSON does not depend on construction order.
 */
export function normalizeLegacyDeliveryOracleValue(value, options) {
  if (typeof value === 'bigint') return value.toString()
  if (typeof value === 'string') return normalizeString(value, options)
  if (Array.isArray(value)) {
    return value.map(entry => normalizeLegacyDeliveryOracleValue(entry, options))
  }
  if (typeof value !== 'object' || value === null) return value

  return Object.fromEntries(Object.keys(value).toSorted().flatMap((key) => {
    const entry = value[key]
    if (entry === undefined) return []
    return [[
      key,
      key === 'proof'
        ? '<AUTH_PROOF>'
        : normalizeLegacyDeliveryOracleValue(entry, options),
    ]]
  }))
}

const ORACLE_SCHEMA_VERSION = 'winwincode.delivery-strongflow-differential-oracle.v1'

const RUNNER_CONTRACT = Object.freeze({
  commandKinds: Object.freeze([
    'strongflow.request',
    'fixture.execution-source.replace',
    'fixture.service.restart',
    'fixture.store.corrupt-record',
    'fixture.store.restore-record',
    'fixture.store.seed-snapshot',
  ]),
  input: 'commands[].request for strongflow.request; commands[].input for fixture commands',
  output: 'commands[].response plus observation snapshot, events, projection, verdict, and store',
  publicInterface: 'StrongFlowDeliveryInvoker.invoke',
  placeholders: Object.freeze({
    '<AUTH_PROOF>': 'runner-supplied fixture authentication proof',
    '<NODE_EXECUTABLE>': 'runner-supplied Node executable',
    '<ORACLE_ROOT>': 'runner-supplied isolated fixture root',
  }),
})

async function createScenarioContext(id) {
  const {
    DeliveryServiceFixtureTestkit,
  } = await import('./delivery-service-testkit.mjs')
  const kit = await DeliveryServiceFixtureTestkit.create({
    deliveryId: oracleDeliveryId(id),
    repositoryLocator: `/workspace/oracle/${id}`,
  })
  const commands = []
  const context = { id, kit, commands }
  attachRecordingInvoker(context)
  return context
}

function attachRecordingInvoker(context) {
  const invoker = context.kit.invoker
  context.kit.invoker = Object.freeze({
    async invoke(request, options) {
      const response = await invoker.invoke(request, options)
      context.commands.push({ kind: 'strongflow.request', request, response })
      return response
    },
  })
}

function recordFixtureCommand(context, kind, input, response = null) {
  context.commands.push({ kind, input, response })
}

function storeObservation(stored) {
  return {
    manifest: stored.manifest,
    records: stored.records.map(record => ({
      digest: record.digest,
      operation: record.operation,
      previousDigest: record.previousDigest,
      requestDigest: record.requestDigest,
      requestId: record.requestId,
      schemaVersion: record.schemaVersion,
      sequence: record.sequence,
      snapshotRevision: record.snapshot.revision,
    })),
    snapshot: stored.snapshot,
  }
}

async function completeScenario(context, assertions = {}) {
  recordFixtureCommand(
    context,
    'fixture.execution-source.replace',
    context.kit.diagramFacts,
    { accepted: true },
  )
  const projectionResponse = await context.kit.request(
    'getDeliveryProjection',
    `oracle:${context.id}:get-projection`,
    { deliveryId: context.kit.deliveryId },
  )
  if (!projectionResponse.ok) {
    throw new Error(
      `${context.id} final projection failed with ${projectionResponse.error.code}`,
    )
  }
  const stored = await context.kit.stored()
  const scenario = normalizeLegacyDeliveryOracleValue({
    assertions,
    commands: context.commands,
    id: context.id,
    observation: {
      events: context.kit.diagramFacts.runtimeEvents,
      projection: {
        diagramExecution: projectionResponse.result.diagramExecution,
        runtimeExecution: projectionResponse.result.runtimeExecution,
      },
      snapshot: projectionResponse.result.delivery,
      store: storeObservation(stored),
      verdict: projectionResponse.result.delivery.verdict,
    },
  }, { root: context.kit.root })
  await context.kit.cleanup()
  return scenario
}

function requireSuccessResponse(response, label) {
  if (!response.ok) throw new Error(`${label} failed with ${response.error.code}`)
  return response.result.delivery
}

async function approvePlan(context) {
  const review = await context.kit.preparePlanReview()
  const approved = await context.kit.approvePlan(review, {
    requestId: `oracle:${context.id}:approve-plan`,
  })
  return requireSuccessResponse(approved, `${context.id} plan approval`)
}

async function successClosedLoopScenario() {
  const context = await createScenarioContext('success-closed-loop')
  try {
    const approved = await approvePlan(context)
    const prepared = await context.kit.prepareCandidateVerification(approved, {
      prefix: 'success',
      value: 'after',
      expectedTestPass: true,
      message: 'Produce successful oracle candidate',
      commitDate: '2025-01-02T00:00:00Z',
    })
    const events = await context.kit.verificationEvents(prepared, {
      reviewer: 'pass',
      verifier: 'pass',
    })
    const verdictResponse = await context.kit.submitVerdict(prepared, events, {
      requestId: 'oracle:success:submit-verdict',
    })
    const readyToDeliver = requireSuccessResponse(verdictResponse, 'successful verdict')
    const review = await context.kit.prepareDeliveryReview(readyToDeliver, {
      prefix: 'success',
    })
    const deliveredResponse = await context.kit.approveDelivery(review, {
      requestId: 'oracle:success:approve-delivery',
    })
    const delivered = requireSuccessResponse(deliveredResponse, 'delivery approval')
    return await completeScenario(context, {
      finalRevision: delivered.revision,
      finalStatus: delivered.status,
      verdict: delivered.verdict?.status ?? null,
    })
  } catch (error) {
    await context.kit.cleanup()
    throw error
  }
}

async function requestIdReplayScenario() {
  const context = await createScenarioContext('request-id-replay')
  try {
    const specification = context.kit.spec(1, 'replay')
    const payload = { spec: specification, tasks: [] }
    const first = await context.kit.request('createDelivery', 'oracle:replay:create', payload)
    const replay = await context.kit.request('createDelivery', 'oracle:replay:create', payload)
    if (!first.ok || !replay.ok) throw new Error('requestId replay fixture did not succeed')
    return await completeScenario(context, {
      replayedSnapshotEqual: JSON.stringify(first.result.delivery)
        === JSON.stringify(replay.result.delivery),
      durableRecordCount: 1,
    })
  } catch (error) {
    await context.kit.cleanup()
    throw error
  }
}

async function revisionConflictScenario() {
  const context = await createScenarioContext('revision-conflict')
  try {
    const created = requireSuccessResponse(await context.kit.request(
      'createDelivery',
      'oracle:revision:create',
      { spec: context.kit.spec(1, 'revision-v1'), tasks: [] },
    ), 'revision create')
    const updated = requireSuccessResponse(await context.kit.request(
      'updateDeliverySpec',
      'oracle:revision:update',
      {
        deliveryId: context.kit.deliveryId,
        expectedRevision: created.revision,
        spec: context.kit.spec(2, 'revision-v2'),
      },
    ), 'revision update')
    const conflict = await context.kit.request(
      'updateDeliverySpec',
      'oracle:revision:stale-update',
      {
        deliveryId: context.kit.deliveryId,
        expectedRevision: created.revision,
        spec: context.kit.spec(3, 'revision-stale'),
      },
    )
    if (conflict.ok) throw new Error('stale revision unexpectedly succeeded')
    return await completeScenario(context, {
      currentRevision: conflict.error.currentRevision,
      errorCode: conflict.error.code,
      snapshotUnchanged: updated.revision === conflict.error.currentRevision,
    })
  } catch (error) {
    await context.kit.cleanup()
    throw error
  }
}

async function corruptionRecoveryScenario() {
  const context = await createScenarioContext('corruption-recovery')
  try {
    const { readFile, writeFile } = await import('node:fs/promises')
    const { join } = await import('node:path')
    const { DeliveryStore } = await import('../../packages/strongflow/dist/index.js')
    const created = requireSuccessResponse(await context.kit.request(
      'createDelivery',
      'oracle:corruption:create',
      { spec: context.kit.spec(1, 'corruption'), tasks: [] },
    ), 'corruption create')
    const store = await DeliveryStore.open(context.kit.home, context.kit.deliveryId)
    const recordPath = join(store.recordsDirectory, '1.json')
    const original = await readFile(recordPath, 'utf8')
    const damaged = JSON.parse(original)
    damaged.snapshot.status = 'ready'
    await writeFile(recordPath, `${JSON.stringify(damaged)}\n`)
    recordFixtureCommand(context, 'fixture.store.corrupt-record', {
      mutation: 'snapshot.status=ready-without-digest-update',
      sequence: '1',
    }, { accepted: true })
    const corruptedRead = await context.kit.request(
      'getDeliveryProjection',
      'oracle:corruption:read-damaged',
      { deliveryId: context.kit.deliveryId },
    )
    if (corruptedRead.ok) throw new Error('damaged store unexpectedly opened')
    await writeFile(recordPath, original)
    recordFixtureCommand(
      context,
      'fixture.store.restore-record',
      { sequence: '1' },
      { accepted: true },
    )
    context.kit.restart()
    attachRecordingInvoker(context)
    recordFixtureCommand(
      context,
      'fixture.service.restart',
      { clock: 'next-deterministic-window' },
      { accepted: true },
    )
    const restored = await context.kit.request(
      'getDeliveryProjection',
      'oracle:corruption:read-restored',
      { deliveryId: context.kit.deliveryId },
    )
    const restoredDelivery = requireSuccessResponse(restored, 'restored projection')
    return await completeScenario(context, {
      corruptedReadError: corruptedRead.error.code,
      restoredSnapshotEqual: JSON.stringify(created) === JSON.stringify(restoredDelivery),
    })
  } catch (error) {
    await context.kit.cleanup()
    throw error
  }
}

function taskFixture(specification, id, blockedByTaskIds = []) {
  return {
    schemaVersion: specification.schemaVersion,
    id,
    deliveryId: specification.deliveryId,
    title: `Oracle task ${id}`,
    goal: `Preserve the ${id} dependency behavior.`,
    acceptanceCriterionIds: [specification.acceptanceCriteria[0].id],
    blockedByTaskIds,
    owner: 'oracle-owner',
    status: 'pending',
  }
}

async function taskDagScenario() {
  const context = await createScenarioContext('task-dag')
  try {
    const {
      STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
      materializeStrongFlowDeliveryRequest,
    } = await import('../../packages/contracts/dist/index.js')
    const { DeliveryStore } = await import('../../packages/strongflow/dist/index.js')
    const specification = context.kit.spec(1, 'task-dag')
    const prerequisite = taskFixture(specification, 'oracle-task-prerequisite')
    const dependent = taskFixture(
      specification,
      'oracle-task-dependent',
      [prerequisite.id],
    )
    const seededSnapshot = {
      schemaVersion: specification.schemaVersion,
      id: context.kit.deliveryId,
      revision: 1,
      status: 'executing',
      spec: specification,
      tasks: [prerequisite, dependent],
      stageRuns: [],
      sessionBindings: [],
      attentionItems: [],
      evidence: [],
      verdict: null,
      createdAtMillis: specification.createdAtMillis,
      updatedAtMillis: specification.createdAtMillis,
    }
    await DeliveryStore.create({
      home: context.kit.home,
      requestId: 'oracle:task-dag:seed',
      requestDigest: 'a'.repeat(64),
      snapshot: seededSnapshot,
    })
    recordFixtureCommand(
      context,
      'fixture.store.seed-snapshot',
      { snapshot: seededSnapshot },
      { accepted: true },
    )
    const blocked = await context.kit.request(
      'startStage',
      'oracle:task-dag:start-dependent',
      {
        deliveryId: context.kit.deliveryId,
        expectedRevision: 1,
        stageRunId: 'oracle-stage-dependent',
        deliveryTaskId: dependent.id,
        stage: 'executing',
        actorType: 'codex',
        role: 'executor',
        attention: null,
      },
    )
    if (blocked.ok) throw new Error('blocked DeliveryTask unexpectedly started')

    const cyclicDeliveryId = oracleDeliveryId('task-dag-cycle')
    const cyclicSpec = {
      ...specification,
      id: 'spec-oracle-task-dag-cycle',
      deliveryId: cyclicDeliveryId,
      acceptanceCriteria: [{
        ...specification.acceptanceCriteria[0],
        id: 'criterion-oracle-task-dag-cycle',
      }],
    }
    const cycleA = taskFixture(cyclicSpec, 'oracle-cycle-a', ['oracle-cycle-b'])
    const cycleB = taskFixture(cyclicSpec, 'oracle-cycle-b', ['oracle-cycle-a'])
    const validRequest = materializeStrongFlowDeliveryRequest(
      'createDelivery',
      'oracle:task-dag:create-cycle',
      { spec: cyclicSpec, tasks: [{ ...cycleA, blockedByTaskIds: [] }, cycleB] },
    )
    const cycleResponse = await context.kit.invoker.invoke({
      ...validRequest,
      schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
      payload: { spec: cyclicSpec, tasks: [cycleA, cycleB] },
    })
    if (cycleResponse.ok) throw new Error('cyclic DeliveryTask graph unexpectedly persisted')
    return await completeScenario(context, {
      blockedTaskError: blocked.error.code,
      cycleError: cycleResponse.error.code,
      durableTaskOrder: seededSnapshot.tasks.map(task => task.id),
    })
  } catch (error) {
    await context.kit.cleanup()
    throw error
  }
}

async function prepareFailedCandidateAndEnterRework(context, prefix = 'failed') {
  const approved = await approvePlan(context)
  const failedCandidate = await context.kit.prepareCandidateVerification(approved, {
    prefix,
    value: 'defect',
    expectedTestPass: false,
    message: `Produce ${prefix} oracle candidate`,
    commitDate: '2025-01-02T00:00:00Z',
  })
  const failingEvents = await context.kit.verificationEvents(failedCandidate, {
    reviewer: 'fail',
    verifier: 'fail',
  })
  const failedResponse = await context.kit.submitVerdict(failedCandidate, failingEvents, {
    requestId: `oracle:${context.id}:submit-failed-verdict`,
  })
  const failedDelivery = requireSuccessResponse(failedResponse, `${context.id} failed verdict`)
  const attention = failedDelivery.attentionItems.find(item => (
    item.status === 'open' && item.options.some(option => option.id === 'start-rework')
  ))
  if (attention === undefined) throw new Error(`${context.id} did not open rework Attention`)
  const reworking = await context.kit.requireSuccess(
    'resolveAttention',
    `oracle:${context.id}:approve-rework`,
    {
      deliveryId: context.kit.deliveryId,
      expectedRevision: failedDelivery.revision,
      attentionItemId: attention.id,
      status: 'resolved',
      resolution: 'Correct the exact failed candidate under the approved DeliverySpec.',
      remediation: null,
      channel: 'local-ui',
      authentication: {
        scheme: 'local-session',
        proof: 'fixture-local-session-proof-value',
      },
    },
  )
  return { failedCandidate, failedDelivery, reworking }
}

async function prepareCorrectedCandidate(context, reworking, prefix = 'corrected') {
  const corrected = await context.kit.prepareCandidateVerification(reworking, {
    prefix,
    writerStage: 'reworking',
    value: 'after',
    expectedTestPass: true,
    message: `Produce ${prefix} oracle candidate`,
    commitDate: '2025-01-03T00:00:00Z',
  })
  const events = await context.kit.verificationEvents(corrected, {
    reviewer: 'pass',
    verifier: 'pass',
  })
  return { corrected, events }
}

async function candidateInvalidationScenario() {
  const context = await createScenarioContext('candidate-invalidation')
  try {
    const failed = await prepareFailedCandidateAndEnterRework(context, 'stale-original')
    const corrected = await prepareCorrectedCandidate(
      context,
      failed.reworking,
      'stale-corrected',
    )
    const stale = await context.kit.submitVerdict(corrected.corrected, corrected.events, {
      requestId: 'oracle:candidate-invalidation:submit-stale',
      candidate: failed.failedCandidate.candidate,
    })
    if (stale.ok) throw new Error('stale candidate unexpectedly produced a Verdict')
    const current = await context.kit.submitVerdict(corrected.corrected, corrected.events, {
      requestId: 'oracle:candidate-invalidation:submit-current',
    })
    requireSuccessResponse(current, 'current candidate verdict')
    return await completeScenario(context, {
      candidateChanged: failed.failedCandidate.candidate.candidateRef
        !== corrected.corrected.candidate.candidateRef,
      staleCandidateError: stale.error.code,
    })
  } catch (error) {
    await context.kit.cleanup()
    throw error
  }
}

async function attentionScenario() {
  const context = await createScenarioContext('attention')
  try {
    const review = await context.kit.preparePlanReview()
    const openAttentionStatus = review.delivery.status
    const approved = await context.kit.approvePlan(review, {
      requestId: 'oracle:attention:approve-plan',
    })
    const resolved = requireSuccessResponse(approved, 'Attention resolution')
    return await completeScenario(context, {
      attentionItemStatus: resolved.attentionItems.at(-1)?.status ?? null,
      openAttentionStatus,
      resolvedStatus: resolved.status,
    })
  } catch (error) {
    await context.kit.cleanup()
    throw error
  }
}

async function outcomeScenario(id, outcomes) {
  const context = await createScenarioContext(id)
  try {
    const approved = await approvePlan(context)
    const prepared = await context.kit.prepareCandidateVerification(approved, {
      prefix: id,
      value: 'after',
      expectedTestPass: true,
      message: `Produce ${id} oracle candidate`,
      commitDate: '2025-01-02T00:00:00Z',
    })
    const events = await context.kit.verificationEvents(prepared, outcomes)
    const submitted = await context.kit.submitVerdict(prepared, events, {
      requestId: `oracle:${id}:submit-verdict`,
    })
    const delivery = requireSuccessResponse(submitted, `${id} verdict`)
    return await completeScenario(context, {
      attentionTypes: delivery.attentionItems
        .filter(item => item.status === 'open')
        .map(item => item.type),
      verdictStatus: delivery.verdict?.status ?? null,
    })
  } catch (error) {
    await context.kit.cleanup()
    throw error
  }
}

async function inconclusiveScenario() {
  return outcomeScenario('inconclusive', {
    reviewer: 'pass',
    verifier: 'inconclusive',
  })
}

async function infraErrorScenario() {
  return outcomeScenario('infra-error', {
    reviewer: 'timed-out',
    verifier: 'timed-out',
  })
}

async function reworkScenario() {
  const context = await createScenarioContext('rework')
  try {
    const failed = await prepareFailedCandidateAndEnterRework(context, 'rework-failed')
    const corrected = await prepareCorrectedCandidate(
      context,
      failed.reworking,
      'rework-corrected',
    )
    const passingResponse = await context.kit.submitVerdict(
      corrected.corrected,
      corrected.events,
      { requestId: 'oracle:rework:submit-corrected-verdict' },
    )
    const passed = requireSuccessResponse(passingResponse, 'corrected rework verdict')
    return await completeScenario(context, {
      candidateChanged: failed.failedCandidate.candidate.candidateRef
        !== corrected.corrected.candidate.candidateRef,
      enteredRework: failed.reworking.status === 'reworking',
      verdicts: [failed.failedDelivery.verdict?.status ?? null, passed.verdict?.status ?? null],
    })
  } catch (error) {
    await context.kit.cleanup()
    throw error
  }
}

const SCENARIO_RUNNERS = Object.freeze({
  'success-closed-loop': successClosedLoopScenario,
  'request-id-replay': requestIdReplayScenario,
  'revision-conflict': revisionConflictScenario,
  'corruption-recovery': corruptionRecoveryScenario,
  'task-dag': taskDagScenario,
  'candidate-invalidation': candidateInvalidationScenario,
  attention: attentionScenario,
  inconclusive: inconclusiveScenario,
  'infra-error': infraErrorScenario,
  rework: reworkScenario,
})

export async function buildLegacyDeliveryStrongFlowOracle(options = {}) {
  const scenarioIds = options.scenarioIds ?? Object.keys(SCENARIO_RUNNERS)
  const unknown = scenarioIds.filter(id => !Object.hasOwn(SCENARIO_RUNNERS, id))
  if (unknown.length > 0) {
    throw new TypeError(`unknown legacy Delivery oracle scenario: ${unknown.join(', ')}`)
  }
  const scenarios = []
  for (const id of scenarioIds) scenarios.push(await SCENARIO_RUNNERS[id]())
  return normalizeLegacyDeliveryOracleValue({
    runnerContract: RUNNER_CONTRACT,
    scenarios,
    schemaVersion: ORACLE_SCHEMA_VERSION,
    source: 'typescript-strongflow-public-invoker',
  }, { root: process.cwd() })
}

export { recordFixtureCommand }
