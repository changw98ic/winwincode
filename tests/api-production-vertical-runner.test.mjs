import assert from 'node:assert/strict'
import {
  chmodSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { resolve } from 'node:path'
import { tmpdir } from 'node:os'
import test from 'node:test'

import {
  prepareControlledRepository,
  appendDeliveryTransition,
  DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY,
  driveDelivery,
  workItemCreatePayload,
  verifyApiProductionSourceSeal,
} from '../scripts/run-api-production-vertical.mjs'

const root = resolve(import.meta.dirname, '..')
const runnerPath = resolve(root, 'scripts/run-api-production-vertical.mjs')
const browserGatePath = resolve(root, 'tests/browser-chat-production.test.mjs')

function deliveryDriverClient(terminalTransitionCount = null) {
  let revision = 0
  let requestSequence = 0
  const commands = []
  const workItem = {
    id: 'wit_01J00000000000000000000001',
    revision: 1,
    state: 'in_progress',
  }
  const workRun = {
    id: 'wrn_01J00000000000000000000001',
    workItemId: workItem.id,
    productSessionId: 'psn_01J00000000000000000000001',
    executionJobId: 'job_01J00000000000000000000001',
    state: 'running',
    attempt: 1,
  }
  const contract = {
    id: 'wct_01J00000000000000000000001',
    revision: 1,
    criteria: [{ id: 'crt_01J00000000000000000000001', required: true }],
  }
  return {
    commands,
    async command(command, previousRevision, payload) {
      if (command === 'workrun.start') {
        assert.equal(payload.dispatchProfile, 'executor')
      }
      if (command === 'workitems.create') {
        assert.equal(payload.expectedRevision, previousRevision)
        assert.equal(payload.items.length, 1)
        assert.deepEqual(payload.items[0].criterionIds, contract.criteria.map(criterion => criterion.id))
      }
      commands.push({ command, previousRevision, payload })
      return {
        command,
        currentRevision: previousRevision + 1,
        outcome: 'completed',
        previousRevision,
      }
    },
    async query(name) {
      if (name === 'workrun.get') {
        const terminal = terminalTransitionCount !== null
          && revision >= terminalTransitionCount
        return {
          result: {
            contract,
            items: [{ ...workItem, state: terminal ? 'done' : 'in_progress' }],
            runs: [{ ...workRun, state: terminal ? 'settled' : 'running' }],
          },
        }
      }
      revision += 1
      const terminal = revision === terminalTransitionCount
      return {
        result: {
          attention: [],
          currentCandidate: terminal
            ? {
                candidateRef: `git-candidate:sha256:${'a'.repeat(64)}`,
                status: 'frozen',
              }
            : null,
          deliveryRevision: revision,
          evidence: terminal ? [{ id: 'evidence-terminal' }] : [],
          status: terminal ? 'done' : 'ready',
          verdict: terminal
            ? { criteria: [{ verdict: 'pass' }], status: 'pass' }
            : null,
        },
      }
    },
    requestId() {
      requestSequence += 1
      return `request-${requestSequence}`
    },
  }
}

test('WorkRun contract drives canonical WorkItem creation payload', async () => {
  const client = deliveryDriverClient()
  const aggregate = (await client.query('workrun.get', {
    deliveryId: 'dlv_01J00000000000000000000001',
    workItemId: null,
  })).result
  const payload = workItemCreatePayload({ ...aggregate, items: [] }, 1)
  assert.equal(payload.deliveryId, 'dlv_01J00000000000000000000001')
  assert.equal(payload.items[0].id, 'wit_01J00000000000000000000001')
  assert.deepEqual(payload.items[0].criterionIds, [
    'crt_01J00000000000000000000001',
  ])
  await client.command('workitems.create', 1, payload)
  await client.command('workrun.start', 2, {
    deliveryId: payload.deliveryId,
    dispatchProfile: 'executor',
  })
})

test('Delivery transition evidence keeps the newest bounded window without limiting progress', () => {
  const trace = { observations: [], totalTransitionCount: 0 }
  const transitionCount = DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY + 5
  for (let revision = 1; revision <= transitionCount; revision += 1) {
    assert.equal(appendDeliveryTransition(trace, { revision, status: 'ready' }), true)
  }
  assert.equal(
    appendDeliveryTransition(trace, { revision: transitionCount, status: 'ready' }),
    false,
  )
  assert.equal(trace.totalTransitionCount, transitionCount)
  assert.equal(trace.observations.length, DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY)
  assert.deepEqual(trace.observations.at(0), { revision: 6, status: 'ready' })
  assert.deepEqual(trace.observations.at(-1), {
    revision: transitionCount,
    status: 'ready',
  })
})

test('Delivery reaches its real terminal state after more transitions than the evidence window', async () => {
  const transitionCount = DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY + 5
  const client = deliveryDriverClient(transitionCount)
  const result = await driveDelivery(client, 10_000)
  assert.equal(result.detail.status, 'done')
  assert.equal(result.totalTransitionCount, transitionCount)
  assert.equal(result.observations.length, DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY)
  assert.deepEqual(result.observations.at(0), { revision: 6, status: 'ready' })
  assert.deepEqual(result.observations.at(-1), {
    revision: transitionCount,
    status: 'done',
  })
  assert.deepEqual(client.commands, [], 'active WorkRuns must be polled, not advanced')
})

test('candidate-ready dispatch sequence tolerates a lagging WorkRun projection', async () => {
  let revision = 7
  let delivered = false
  let instant = 0
  let requestSequence = 0
  const commands = []
  const client = {
    async query(name) {
      if (name === 'workrun.get') {
        return {
          result: {
            items: [{ state: delivered ? 'done' : 'candidate_ready' }],
            runs: [{
              id: 'wrn_00000000000000000000000001',
              workItemId: 'wit_01J00000000000000000000001',
              executionJobId: 'job_00000000000000000000000001',
              state: delivered ? 'settled' : 'candidate_ready',
            }],
          },
        }
      }
      return {
        result: {
          attention: [],
          currentCandidate: {
            candidateRef: `git-candidate:sha256:${'a'.repeat(64)}`,
          },
          deliveryRevision: revision,
          evidence: delivered ? [{ id: 'evidence-terminal' }] : [],
          readCursor: null,
          status: delivered ? 'done' : 'backlog',
          verdict: delivered
            ? { criteria: [{ verdict: 'pass' }], status: 'pass' }
            : null,
        },
      }
    },
    async command(command, previousRevision, payload) {
      commands.push({ command, profile: payload.dispatchProfile ?? null })
      if (command === 'delivery.submit_verdict') delivered = true
      revision = previousRevision + 1
      return { command, currentRevision: revision, outcome: 'completed', previousRevision }
    },
    requestId() {
      requestSequence += 1
      return `request-${requestSequence}`
    },
  }

  await driveDelivery(client, 10, undefined, () => instant++)
  assert.deepEqual(commands, [
    { command: 'workrun.start', profile: 'reviewer' },
    { command: 'workrun.start', profile: 'verifier' },
    { command: 'delivery.submit_verdict', profile: null },
  ])
})

test('Delivery timeout reports the total count and only the newest transition window', async () => {
  let instant = 0

  await assert.rejects(
    () => driveDelivery(
      deliveryDriverClient(),
      DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY + 1,
      undefined,
      () => instant++,
    ),
    error => {
      const prefix = 'StrongFlow did not reach done: '
      assert.equal(error.message.startsWith(prefix), true)
      const diagnostic = JSON.parse(error.message.slice(prefix.length))
      assert.equal(
        diagnostic.totalTransitionCount,
        DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY + 1,
      )
      assert.equal(
        diagnostic.observations.length,
        DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY,
      )
      assert.deepEqual(diagnostic.observations.at(0), { revision: 2, status: 'ready' })
      assert.deepEqual(diagnostic.observations.at(-1), {
        revision: DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY + 1,
        status: 'ready',
      })
      return true
    },
  )
})

test('API production vertical is a direct generated HTTP runner', async () => {
  const source = readFileSync(runnerPath, 'utf8')
  for (const endpoint of ['/api/v1/auth/session', '/api/v1/commands', '/api/v1/queries']) {
    assert.equal(source.includes(endpoint), true, `runner must call ${endpoint}`)
  }
  for (const operation of [
    'session.create',
    'session.cancel',
    'chat.submit',
    'delivery.create',
    'workitems.create',
    'workrun.start',
    'delivery.resolve_attention',
    'delivery.submit_verdict',
    'delivery.get',
    'runtime.projection.get',
    'workrun.get',
  ]) {
    assert.equal(source.includes(operation), true, `runner must cover ${operation}`)
  }
  assert.match(source, /\/health/u)
  assert.match(source, /httpsRequest/u)
  assert.match(source, /CARGO_TARGET_DIR/u)
  assert.match(source, /deliveryFailureSummary/u)
  assert.match(source, /workrun\.get/u)
  assert.match(source, /dispatchProfile/u)
  assert.match(source, /providerRoute/u)
  assert.match(source, /candidateArtifact/u)
  assert.match(source, /workitems\.create/u)
  assert.match(source, /GIT_CONFIG_NOSYSTEM/u)
  assert.match(source, /API_SOURCE_SEAL_NAME/u)
  assert.match(source, /apiProductionSourceDigest/u)
  assert.match(source, /writeApiProductionSourceSeal/u)
  assert.match(source, /verifyApiProductionSourceSeal/u)
  assert.match(source, /trackedDiffSha256/u)
  assert.match(source, /helperReleaseManifestMode/u)
  assert.match(source, /source seal missing or invalid/u)
  assert.match(source, /IP\.1 = 127\.0\.0\.1/u)
  assert.match(source, /WWC_WORKER_SERVER_ORIGIN: controlUrl/u)
  assert.doesNotMatch(source, /controlUrl\.replace\('127\.0\.0\.1'/u)
  assert.match(source, /export async function runApiProductionVertical/u)
  assert.doesNotMatch(source, /approveSolutionResolution/u)
  assert.match(source, /kind: 'work-run'/u)
  assert.doesNotMatch(source, /\b(?:chromium|devtools|document|window|WebSocket)\b/iu)
})

test('skip-build rejects an old target before any API process starts', () => {
  const target = mkdtempSync(resolve(tmpdir(), 'winwincode-api-source-seal-test-'))
  const debug = resolve(target, 'debug')
  mkdirSync(debug, { recursive: true })
  const serverBinary = resolve(debug, 'winwincode-server')
  const helperExecutable = resolve(debug, 'winwincode-kernel-helper')
  writeFileSync(serverBinary, '#!/bin/sh\nexit 0\n')
  writeFileSync(helperExecutable, '#!/bin/sh\nexit 0\n')
  chmodSync(serverBinary, 0o755)
  chmodSync(helperExecutable, 0o755)
  try {
    assert.throws(
      () => verifyApiProductionSourceSeal({
        root,
        serverBinary,
        helperExecutable,
      }),
      /source seal missing or invalid/u,
    )
  } finally {
    rmSync(target, { recursive: true, force: true })
  }
})

test('browser skip-build verifies the production source seal before replacing artifacts', () => {
  const source = readFileSync(browserGatePath, 'utf8')
  const verification = source.indexOf('verifyApiProductionSourceSeal({')
  const temporaryDirectory = source.indexOf("mkdtempSync(join(tmpdir(), 'winwincode-browser-product-'))")
  const artifactReplacement = source.indexOf('rmSync(artifactDirectory, { recursive: true, force: true })')
  assert.notEqual(verification, -1, 'browser gate must verify the API production source seal')
  assert.ok(verification < temporaryDirectory, 'source verification must precede temporary resources')
  assert.ok(verification < artifactReplacement, 'source verification must preserve prior artifacts on failure')
})


test('production scenario files enter the committed repository and reject path escape', () => {
  const directory = mkdtempSync(resolve(tmpdir(), 'wwc-ui-scenario-'))
  try {
    const { repository } = prepareControlledRepository({ fixtureDirectory: directory, files: { 'index.html': '<h1>UI</h1>' } })
    assert.equal(readFileSync(resolve(repository, 'index.html'), 'utf8'), '<h1>UI</h1>')
    assert.throws(() => prepareControlledRepository({ fixtureDirectory: resolve(directory, 'invalid'), files: { '../outside.html': 'bad' } }), /must stay inside/u)
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
})
