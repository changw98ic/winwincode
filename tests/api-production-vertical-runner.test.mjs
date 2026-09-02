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
  appendDeliveryTransition,
  DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY,
  driveDelivery,
  verifyApiProductionSourceSeal,
} from '../scripts/run-api-production-vertical.mjs'

const root = resolve(import.meta.dirname, '..')
const runnerPath = resolve(root, 'scripts/run-api-production-vertical.mjs')
const browserGatePath = resolve(root, 'tests/browser-chat-strongflow-production.test.mjs')

function deliveryDriverClient(terminalTransitionCount = null) {
  let revision = 0
  let requestSequence = 0
  const terminalStages = ['planner', 'executor', 'reviewer', 'verifier'].map(role => {
    const id = `stage-${role}`
    const productSessionId = `session-${id}`
    const workerSessionId = `worker-session-${id}`
    const codexThreadId = `codex-${id}`
    return {
      actorType: 'codex',
      attempt: 1,
      id,
      role,
      sessionBinding: {
        codexThreadId,
        executionJobId: `job-${id}`,
        productSessionId,
        sessionIdentity: {
          codexThreadId,
          productSessionId,
          stageRunId: id,
          workerSessionId,
        },
        workerId: `worker-${id}`,
        workerSessionId,
      },
      status: 'succeeded',
    }
  })
  return {
    async command(command, previousRevision) {
      return {
        command,
        currentRevision: previousRevision + 1,
        outcome: 'completed',
        previousRevision,
      }
    },
    async query() {
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
          stages: terminal ? terminalStages : [],
          status: terminal ? 'delivered' : 'executing',
          tasks: terminal ? [{ status: 'completed' }] : [],
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

test('Delivery transition evidence keeps the newest bounded window without limiting progress', () => {
  const trace = { observations: [], totalTransitionCount: 0 }
  const transitionCount = DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY + 5
  for (let revision = 1; revision <= transitionCount; revision += 1) {
    assert.equal(appendDeliveryTransition(trace, { revision, status: 'executing' }), true)
  }
  assert.equal(
    appendDeliveryTransition(trace, { revision: transitionCount, status: 'executing' }),
    false,
  )
  assert.equal(trace.totalTransitionCount, transitionCount)
  assert.equal(trace.observations.length, DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY)
  assert.deepEqual(trace.observations.at(0), { revision: 6, status: 'executing' })
  assert.deepEqual(trace.observations.at(-1), {
    revision: transitionCount,
    status: 'executing',
  })
})

test('Delivery reaches its real terminal state after more transitions than the evidence window', async () => {
  const transitionCount = DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY + 5
  const result = await driveDelivery(deliveryDriverClient(transitionCount), 10_000)
  assert.equal(result.detail.status, 'delivered')
  assert.equal(result.totalTransitionCount, transitionCount)
  assert.equal(result.observations.length, DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY)
  assert.deepEqual(result.observations.at(0), { revision: 6, status: 'executing' })
  assert.deepEqual(result.observations.at(-1), {
    revision: transitionCount,
    status: 'delivered',
  })
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
      const prefix = 'StrongFlow did not reach delivered: '
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
      assert.deepEqual(diagnostic.observations.at(0), { revision: 2, status: 'executing' })
      assert.deepEqual(diagnostic.observations.at(-1), {
        revision: DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY + 1,
        status: 'executing',
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
    'delivery.advance',
    'delivery.resolve_attention',
    'delivery.approve_task_breakdown',
    'delivery.submit_verdict',
    'delivery.get',
    'runtime.projection.get',
  ]) {
    assert.equal(source.includes(operation), true, `runner must cover ${operation}`)
  }
  assert.match(source, /\/health/u)
  assert.match(source, /httpsRequest/u)
  assert.match(source, /CARGO_TARGET_DIR/u)
  assert.match(source, /deliveryFailureSummary/u)
  assert.match(source, /stageRuns/u)
  assert.match(source, /providerRoute/u)
  assert.match(source, /candidateArtifact/u)
  assert.match(source, /GIT_CONFIG_NOSYSTEM/u)
  assert.match(source, /API_SOURCE_SEAL_NAME/u)
  assert.match(source, /apiProductionSourceDigest/u)
  assert.match(source, /writeApiProductionSourceSeal/u)
  assert.match(source, /verifyApiProductionSourceSeal/u)
  assert.match(source, /trackedDiffSha256/u)
  assert.match(source, /helperReleaseManifestMode/u)
  assert.match(source, /source seal missing or invalid/u)
  assert.match(source, /export async function runApiProductionVertical/u)
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
