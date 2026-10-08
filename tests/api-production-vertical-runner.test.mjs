import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
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
import { projectSourceDigest } from '../scripts/product-build-contract.mjs'
import { loadDeviceAgentTask } from '../scripts/device-agent-task.mjs'

import {
  prepareControlledRepository,
  probeDeviceErrorTruthfulness,
  stopDevicePath,
  appendDeliveryTransition,
  DELIVERY_TRANSITION_DIAGNOSTIC_CAPACITY,
  driveDelivery,
  workItemCreatePayload,
  verifyApiProductionSourceSeal,
  writeApiProductionSourceSeal,
  writeHelperReleaseManifest,
  runApiProductionVertical,
  waitForDeviceWorkerRegistered,
} from '../scripts/run-api-production-vertical.mjs'

const root = resolve(import.meta.dirname, '..')
const runnerPath = resolve(root, 'scripts/run-api-production-vertical.mjs')
const browserGatePath = resolve(root, 'tests/browser-chat-production.test.mjs')

test('registration reads the exact Worker beyond a full historical first page', async () => {
  const historical = Array.from({ length: 201 }, (_, index) => ({ id: `old-${index}`, state: 'drained' }))
  const target = { id: 'target', state: 'enabled', lastHeartbeatAt: '2026-10-03T00:00:00Z' }
  const queries = []
  const api = { async query(name, parameters, pagination) {
    queries.push(name)
    if (name === 'worker.list') return { result: { items: historical.slice(0, 200) },
      page: { hasMore: true, nextCursor: 'later' } }
    assert.equal(name, 'worker.get')
    assert.deepEqual(parameters, { workerId: target.id })
    assert.deepEqual(pagination, { cursor: null, limit: 1 })
    return { result: target }
  } }
  assert.deepEqual(await waitForDeviceWorkerRegistered(api, { workerId: target.id }, 1), target)
  assert.deepEqual(queries, ['worker.get'])
})

test('external Provider without a credential fails before build or deterministic fallback', async () => {
  await assert.rejects(runApiProductionVertical({
    deviceProvider: {
      providerId: 'live-model', modelId: 'live-model', endpoint: 'https://provider.invalid/v1/messages',
    },
  }), /External Device Provider requires a credential/u)
})

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

test('Delivery helpers keep each task on its own canonical Delivery identity', async () => {
  const deliveryId = 'dlv_01J00000000000000000000003'
  const client = deliveryDriverClient(1)
  const query = client.query.bind(client)
  client.query = (name, payload) => {
    assert.equal(payload.deliveryId, deliveryId)
    return query(name, payload)
  }
  const aggregate = { contract: { revision: 1, criteria: [{ id: 'criterion' }] }, items: [],
    readCursor: { deliveryId } }
  assert.equal(workItemCreatePayload(aggregate, 1).deliveryId, deliveryId)
  assert.equal(workItemCreatePayload(aggregate, 1).items[0].id, 'wit_01J00000000000000000000003')
  const result = await driveDelivery(client, 10_000, undefined, Date.now, { deliveryId })
  assert.equal(result.detail.status, 'done')
})

function completedDeliveryObservation(deliveryId, token = 'terminal') {
  const readCursor = { deliveryId, token }
  return {
    detail: { deliveryId, readCursor, deliveryRevision: 44, status: 'done', attention: [],
      currentCandidate: { candidateRef: 'frozen-candidate' },
      evidence: [{ id: 'real-command-evidence' }],
      verdict: { status: 'pass', criteria: [{ verdict: 'pass' }] } },
    workRunAggregate: { readCursor, items: [{ id: 'item', state: 'done' }],
      runs: [{ id: 'run', workItemId: 'item', executionJobId: 'job', state: 'settled' }] },
  }
}

test('temporary Delivery and cursor-bound WorkRun 503 queries re-read the same task before accepting completion', async () => {
  const deliveryId = 'dlv_01J00000000000000000000003'
  const terminal = completedDeliveryObservation(deliveryId)
  const stale = { ...terminal.detail, deliveryRevision: 43, status: 'waiting_human',
    readCursor: { deliveryId, token: 'unavailable-cursor' },
    attention: [{ id: 'stale-approval', status: 'open' }] }
  const original = structuredClone({ stale, terminal })
  const queries = []
  const projections = []
  const unavailable = () => Object.assign(new Error('Trusted facts are temporarily unavailable'), {
    code: 'TRUSTED_FACTS_UNAVAILABLE', status: 503,
  })
  const client = {
    async query(name, parameters) {
      queries.push({ name, parameters })
      assert.equal(parameters.deliveryId, deliveryId)
      switch (queries.length) {
        case 1:
          assert.equal(name, 'delivery.get')
          throw unavailable()
        case 2:
          assert.equal(name, 'delivery.get')
          return { result: stale }
        case 3:
          assert.equal(name, 'workrun.get')
          assert.deepEqual(parameters.atCursor, stale.readCursor)
          throw unavailable()
        case 4:
          assert.equal(name, 'delivery.get')
          return { result: terminal.detail }
        case 5:
          assert.equal(name, 'workrun.get')
          assert.deepEqual(parameters.atCursor, terminal.detail.readCursor)
          return { result: terminal.workRunAggregate }
        default:
          assert.fail('completion must use its successful cursor-bound pair')
      }
    },
    command: () => assert.fail('query recovery must not issue a product command'),
    requestId: () => assert.fail('query recovery must not create a command request'),
  }
  const result = await driveDelivery(client, null, undefined, Date.now, {
    deliveryId, resolveAttention: false,
    onProjection: projection => projections.push(projection),
    onActiveWorkRuns: () => assert.fail('unavailable observations must not launch work'),
  })
  assert.deepEqual(projections, [terminal])
  assert.equal(result.detail, terminal.detail)
  assert.equal(result.workRunAggregate, terminal.workRunAggregate)
  assert.deepEqual(result.actions, [])
  assert.deepEqual(result.observations, [{ revision: 44, status: 'done' }])
  assert.deepEqual({ stale, terminal }, original, 'query recovery must not rewrite product evidence')
})

test('uncapped Delivery query recovery is not limited by the local stalled-projection budget', async () => {
  const deliveryId = 'dlv_01J00000000000000000000003'
  const terminal = completedDeliveryObservation(deliveryId)
  let failures = 85
  let acceptedQueries = 0
  const client = {
    async query(name) {
      if (failures-- > 0) {
        assert.equal(name, 'delivery.get')
        throw Object.assign(new Error('Trusted facts are temporarily unavailable'), {
          code: 'TRUSTED_FACTS_UNAVAILABLE', status: 503,
        })
      }
      acceptedQueries += 1
      return { result: name === 'delivery.get' ? terminal.detail : terminal.workRunAggregate }
    },
    command: () => assert.fail('repeated query unavailability cannot dispatch work'),
  }
  const result = await driveDelivery(client, null, undefined, () => Number.MAX_SAFE_INTEGER, { deliveryId })
  assert.equal(result.detail, terminal.detail)
  assert.equal(acceptedQueries, 2)
  assert.equal(result.totalTransitionCount, 1)
})

test('temporary query unavailability respects the existing deadline and retains its typed unresolved cause', async () => {
  const deliveryId = 'dlv_01J00000000000000000000003'
  const terminal = completedDeliveryObservation(deliveryId)
  for (const failedQuery of ['delivery.get', 'workrun.get']) {
    let instant = 0
    let queries = 0
    const unavailable = Object.assign(new Error('Trusted facts are temporarily unavailable'), {
      code: 'TRUSTED_FACTS_UNAVAILABLE', status: 503,
    })
    const client = {
      async query(name) {
        queries += 1
        if (name === failedQuery) throw unavailable
        return { result: terminal.detail }
      },
      command: () => assert.fail('an unresolved deadline cannot dispatch work'),
    }
    await assert.rejects(driveDelivery(client, 1, undefined, () => instant++, {
      deliveryId, onProjection: () => assert.fail('a partial query pair is not evidence'),
    }), error => {
      assert.equal(error, unavailable)
      assert.equal(error.status, 503)
      assert.equal(error.code, 'TRUSTED_FACTS_UNAVAILABLE')
      assert.equal(error.unresolvedDeviceExecution, true)
      return true
    })
    assert.equal(queries, failedQuery === 'delivery.get' ? 1 : 2)
  }
})

test('Delivery query recovery does not swallow invalid queries or unrelated transport failures', async () => {
  const deliveryId = 'dlv_01J00000000000000000000003'
  const terminal = completedDeliveryObservation(deliveryId)
  for (const failedQuery of ['delivery.get', 'workrun.get']) {
    for (const fields of [
      { code: 'INVALID_QUERY', status: 503 },
      { code: 'TRUSTED_FACTS_UNAVAILABLE', status: 400 },
      { code: 'ECONNRESET' },
    ]) {
      let queries = 0
      const failure = Object.assign(new Error('query failed'), fields)
      const client = {
        async query(name) {
          queries += 1
          if (name === failedQuery) throw failure
          return { result: terminal.detail }
        },
        command: () => assert.fail('invalid queries cannot dispatch work'),
      }
      await assert.rejects(driveDelivery(client, null, undefined, Date.now, {
        deliveryId, onProjection: () => assert.fail('failed queries are not evidence'),
      }), error => error === failure)
      assert.equal(queries, failedQuery === 'delivery.get' ? 1 : 2)
    }
  }
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

test('candidate-ready lagging projection waits for the Controller without local dispatch', async () => {
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
      // The lagging WorkRun projection eventually catches up to done without
      // the runner inventing workrun.start / submit_verdict commands.
      if (instant >= 3) delivered = true
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
    async command(command) {
      commands.push({ command })
      return {
        command,
        currentRevision: revision,
        outcome: 'completed',
        previousRevision: revision,
      }
    },
    requestId() {
      requestSequence += 1
      return `request-${requestSequence}`
    },
  }

  await driveDelivery(client, 20, undefined, () => instant++)
  assert.deepEqual(commands, [], 'Controller owns verification and verdict dispatch')
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
    // Controller owns delivery.submit_verdict after verifier terminal.
    'delivery.get',
    'runtime.projection.get',
    'workrun.get',
  ]) {
    assert.equal(source.includes(operation), true, `runner must cover ${operation}`)
  }
  assert.equal(
    source.includes('delivery.submit_verdict'),
    false,
    'Controller owns submit_verdict; the runner must not dual-dispatch it',
  )
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

test('source seal binds the Device Worker and CLI binaries', () => {
  const target = mkdtempSync(resolve(tmpdir(), 'winwincode-api-full-source-seal-'))
  const serverBinary = resolve(target, 'winwincode-server')
  const helperExecutable = resolve(target, 'winwincode-kernel-helper')
  const workerBinary = resolve(target, 'winwincode-worker')
  const cliBinary = resolve(target, 'wwc')
  try {
    for (const path of [serverBinary, helperExecutable, workerBinary, cliBinary]) {
      writeFileSync(path, '#!/bin/sh\nexit 0\n')
      chmodSync(path, 0o755)
    }
    const helperReleaseManifest = writeHelperReleaseManifest(root, helperExecutable)
    const sealed = writeApiProductionSourceSeal({ root, serverBinary, helperExecutable,
      helperReleaseManifest })
    assert.equal(sealed.seal.workerBinaryPath, 'winwincode-worker')
    assert.equal(sealed.seal.cliBinaryPath, 'wwc')
    verifyApiProductionSourceSeal({ root, serverBinary, helperExecutable })
    writeFileSync(workerBinary, '#!/bin/sh\nexit 1\n')
    assert.throws(() => verifyApiProductionSourceSeal({ root, serverBinary, helperExecutable }),
      /Device Worker binary digest changed/u)
    writeFileSync(workerBinary, '#!/bin/sh\nexit 0\n')
    writeFileSync(cliBinary, '#!/bin/sh\nexit 1\n')
    assert.throws(() => verifyApiProductionSourceSeal({ root, serverBinary, helperExecutable }),
      /Device CLI binary digest changed/u)
  } finally { rmSync(target, { recursive: true, force: true }) }
})

test('source seal rejects a changed helper build script at the same Git HEAD', t => {
  const directory = mkdtempSync(resolve(tmpdir(), 'winwincode-api-build-source-seal-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const sourceRoot = resolve(directory, 'source')
  const binaryRoot = resolve(directory, 'bin')
  mkdirSync(resolve(sourceRoot, 'scripts'), { recursive: true })
  mkdirSync(resolve(sourceRoot, 'crates/helper/src'), { recursive: true })
  mkdirSync(binaryRoot)
  writeFileSync(resolve(sourceRoot, 'package.json'), JSON.stringify({ version: '0.1.0' }))
  writeFileSync(resolve(sourceRoot, 'Cargo.toml'), '[workspace.package]\nversion = "0.1.0"\n')
  writeFileSync(resolve(sourceRoot, 'crates/helper/src/main.rs'), 'fn main() {}\n')
  for (const name of ['run-api-production-vertical.mjs', 'product-build-contract.mjs', 'compact-kernel-helper.mjs']) {
    writeFileSync(resolve(sourceRoot, 'scripts', name), '// build input\n')
  }
  const git = (...args) => execFileSync('git', ['-C', sourceRoot, ...args], { encoding: 'utf8' }).trim()
  git('init', '--quiet')
  git('add', '.')
  git('-c', 'user.name=Source Seal Test', '-c', 'user.email=source-seal@example.invalid',
    '-c', 'commit.gpgSign=false', 'commit', '--quiet', '-m', 'fixture')
  const head = git('rev-parse', 'HEAD')
  for (const name of ['winwincode-server', 'winwincode-kernel-helper', 'winwincode-worker', 'wwc']) {
    const path = resolve(binaryRoot, name)
    writeFileSync(path, '#!/bin/sh\nexit 0\n')
    chmodSync(path, 0o755)
  }
  const serverBinary = resolve(binaryRoot, 'winwincode-server')
  const helperExecutable = resolve(binaryRoot, 'winwincode-kernel-helper')
  const options = { root: sourceRoot, serverBinary, helperExecutable }
  writeApiProductionSourceSeal({ ...options,
    helperReleaseManifest: writeHelperReleaseManifest(sourceRoot, helperExecutable) })
  verifyApiProductionSourceSeal(options)
  writeFileSync(resolve(sourceRoot, 'scripts/compact-kernel-helper.mjs'), '// changed build input\n')
  assert.equal(git('rev-parse', 'HEAD'), head)
  assert.throws(() => verifyApiProductionSourceSeal(options), /source seal is stale/u)
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

test('the retained controlled repository reopens without writing or replacing its baseline', t => {
  const directory = mkdtempSync(resolve(tmpdir(), 'wwc-reopen-repository-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const options = { fixtureDirectory: directory, files: { 'TASK.md': 'frozen task\n' } }
  const first = prepareControlledRepository(options)
  const git = (...args) => execFileSync('git', ['-C', first.repository, ...args], { encoding: 'utf8' }).trim()
  const objects = git('count-objects', '-v')
  assert.deepEqual(prepareControlledRepository(options), first)
  assert.equal(git('count-objects', '-v'), objects)
  assert.throws(() => prepareControlledRepository({ ...options, files: { 'TASK.md': 'changed\n' } }),
    /retained.*(input|baseline)/u)
  assert.equal(readFileSync(resolve(first.repository, 'TASK.md'), 'utf8'), 'frozen task\n')
  writeFileSync(resolve(first.repository, 'TASK.md'), 'retained dirty evidence\n')
  assert.throws(() => prepareControlledRepository(options), /retained.*dirty/u)
  assert.equal(readFileSync(resolve(first.repository, 'TASK.md'), 'utf8'), 'retained dirty evidence\n')
  assert.equal(git('rev-parse', 'HEAD'), first.revision)
})


test('explicit Device task input validates repository boundaries and preserves task content', t => {
  const directory = mkdtempSync(resolve(tmpdir(), 'wwc-device-input-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const path = resolve(directory, 'task.json')
  const task = {
    title: 'Frozen task', goal: 'Implement main.py', scope: ['main.py'], constraints: [], outOfScope: [],
    files: { 'main.py': 'print("starter")\n' }, verificationCommand: 'python3 verify.py',
    acceptanceCriteria: [{ id: 'public-example', title: 'Public example passes', required: true }],
  }
  writeFileSync(path, JSON.stringify(task))
  const loaded = loadDeviceAgentTask(path)
  assert.deepEqual(loaded.task, task)
  assert.match(loaded.digest, /^[a-f0-9]{64}$/u)
  for (const files of [{ '../escape': '' }, { '/escape': '' }, { '.git/config': '' }, { 'a/../../escape': '' }, { 'a': 1 }]) {
    writeFileSync(path, JSON.stringify({ ...task, files }))
    assert.throws(() => loadDeviceAgentTask(path))
  }
  writeFileSync(path, JSON.stringify({ ...task, acceptanceCriteria: [...task.acceptanceCriteria, ...task.acceptanceCriteria] }))
  assert.throws(() => loadDeviceAgentTask(path), /unique/u)
})

test('uncapped Device task stops on attention without automatically resolving it', async () => {
  const detail = { deliveryRevision: 1, status: 'running', attention: [{ id: 'attention', status: 'open' }] }
  let commands = 0
  const client = {
    query: async name => ({ result: name === 'delivery.get' ? detail : { runs: [] } }),
    command: async () => { commands += 1 },
  }
  await assert.rejects(driveDelivery(client, null, undefined, () => Number.MAX_SAFE_INTEGER, {
    resolveAttention: false,
    expectDeviceWorkRun: true,
  }), error => error.code === 'DEVICE_TASK_ATTENTION')
  assert.equal(commands, 0)
})

test('terminal Device WorkRuns cannot keep the initial launch wait alive', async () => {
  let polls = 0
  const runs = [{ id: 'executor', state: 'candidate_ready' }, { id: 'verifier', state: 'failed' }]
  const client = {
    query: async name => ({ result: name === 'delivery.get'
      ? { deliveryRevision: 17, status: 'draft', attention: [] }
      : { runs } }),
  }
  await assert.rejects(driveDelivery(client, null, undefined, Date.now, {
    expectDeviceWorkRun: true,
    pendingDeviceWorkRunIds: () => runs.map(run => run.id),
    assertRunning: () => {
      if (++polls > 85) throw new Error('terminal runs are still waiting for launch')
    },
  }), /StrongFlow stalled without a dispatchable next action/u)
})

test('terminal product failure stops the Device driver after saving its final projection', async () => {
  const cursor = { token: 'failed-cursor' }
  const detail = { deliveryRevision: 7, status: 'failed', readCursor: cursor, attention: [] }
  const workRunAggregate = { readCursor: cursor,
    items: [{ state: 'failed' }], runs: [{ id: 'executor', state: 'failed' }] }
  const projections = []
  const client = {
    query: async name => ({ result: name === 'delivery.get' ? detail : workRunAggregate }),
    command: () => assert.fail('terminal failure must not issue another command'),
  }
  await assert.rejects(driveDelivery(client, null, undefined, Date.now, {
    onProjection: projection => projections.push(projection),
  }), { code: 'DEVICE_PRODUCT_FAILED' })
  assert.deepEqual(projections, [{ detail, workRunAggregate }])
})

test('Device runner confirms only the current verified candidate without publication', async () => {
  const candidate = { candidateId: 'candidate', candidateRef: 'ref', deliverySpecId: 'spec', deliverySpecRevision: 1, producerWorkRunId: 'writer' }
  const initial = {
    deliveryRevision: 18, status: 'needs-attention',
    requirements: { deliverySpecId: 'spec', deliverySpecRevision: 1, publicationTarget: null },
    currentCandidate: candidate,
    verdict: { ...candidate, status: 'pass', criteria: [{ verdict: 'pass' }] },
    evidence: [{ id: 'command-evidence' }],
    attention: [{ id: 'approval', type: 'delivery_approval', status: 'open', deliverySpecId: 'spec', workRunId: 'writer' }],
  }
  const cases = [
    [true, () => {}],
    [false, d => { d.attention[0].type = 'scope_change' }],
    [false, d => { d.attention[0].workRunId = 'another-writer' }],
    [false, d => { d.attention[0].deliverySpecId = 'another-spec' }],
    [false, d => { d.attention.push({ id: 'other', status: 'open' }) }],
    [false, d => { d.verdict.status = 'fail' }],
    [false, d => { d.verdict.candidateId = 'other-candidate' }],
    [false, d => { d.verdict.candidateRef = 'other-ref' }],
    [false, d => { d.verdict.deliverySpecRevision = 2 }],
    [false, d => { d.verdict.deliverySpecId = 'other-spec' }],
    [false, d => { d.requirements.deliverySpecRevision = 2 }],
    [false, d => { d.requirements.deliverySpecId = 'other-spec' }],
    [false, d => { d.requirements.publicationTarget = { kind: 'pull-request' } }],
    [false, d => { delete d.requirements.publicationTarget }],
    [false, d => { d.currentCandidate = null }],
    [false, d => { d.verdict = null }],
  ]
  for (const [allowed, mutate] of cases) {
    const detail = structuredClone(initial)
    mutate(detail)
    const commands = []
    const client = {
      query: async name => ({ result: name === 'delivery.get' ? detail : {
        items: [{ id: 'item', state: detail.status === 'done' ? 'done' : 'candidate_ready' }],
        runs: [{ id: 'writer', workItemId: 'item', executionJobId: 'job', state: 'candidate_ready' }],
      } }),
      requestId: () => 'request',
      command: async (command, revision, payload) => {
        commands.push({ command, revision, payload })
        detail.status = 'done'
        detail.attention[0].status = 'resolved'
        detail.deliveryRevision += 1
        return { command, outcome: 'completed', previousRevision: revision, currentRevision: revision + 1 }
      },
    }
    const run = driveDelivery(client, null, undefined, Date.now, { resolveAttention: 'verified-candidate' })
    if (allowed) {
      assert.equal((await run).detail.status, 'done')
      assert.equal(commands.length, 1)
      assert.equal(commands[0].command, 'delivery.resolve_attention')
      assert.equal(commands[0].revision, 18)
      assert.equal(commands[0].payload.attentionItemId, 'approval')
    } else {
      await assert.rejects(run, error => error.code === 'DEVICE_TASK_ATTENTION')
      assert.equal(commands.length, 0)
    }
  }
})

test('product source identity includes embedded Core while excluding build outputs', t => {
  const fixture = mkdtempSync(resolve(tmpdir(), 'wwc-core-source-'))
  t.after(() => rmSync(fixture, { recursive: true, force: true }))
  const source = resolve(fixture, 'third_party/codex/codex-rs/core/src')
  const target = resolve(fixture, 'third_party/codex/codex-rs/target')
  mkdirSync(source, { recursive: true })
  mkdirSync(target, { recursive: true })
  writeFileSync(resolve(source, 'lib.rs'), 'pub const VERSION: u32 = 1;\n')
  const original = projectSourceDigest(fixture)
  writeFileSync(resolve(target, 'generated'), 'build output')
  assert.equal(projectSourceDigest(fixture), original)
  writeFileSync(resolve(source, 'lib.rs'), 'pub const VERSION: u32 = 2;\n')
  assert.notEqual(projectSourceDigest(fixture), original)
})


test('source seal rejects changes in every locally patched Cargo dependency', t => {
  const directory = mkdtempSync(resolve(tmpdir(), 'winwincode-api-vendor-source-seal-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const sourceRoot = resolve(directory, 'source')
  const binaryRoot = resolve(directory, 'bin')
  mkdirSync(resolve(sourceRoot, 'scripts'), { recursive: true })
  mkdirSync(resolve(sourceRoot, 'crates/helper/src'), { recursive: true })
  mkdirSync(binaryRoot)
  writeFileSync(resolve(sourceRoot, 'package.json'), JSON.stringify({ version: '0.1.0' }))
  writeFileSync(resolve(sourceRoot, 'Cargo.toml'), '[workspace.package]\nversion = "0.1.0"\n[patch.crates-io]\nrusqlite = { path = "upstream/vendor/rusqlite-0.39.0" }\ni18n-embed-fl = { path = "upstream/vendor/i18n-embed-fl-0.9.4" }\n')
  writeFileSync(resolve(sourceRoot, 'crates/helper/src/main.rs'), 'fn main() {}\n')
  for (const name of ['run-api-production-vertical.mjs', 'product-build-contract.mjs', 'compact-kernel-helper.mjs']) {
    writeFileSync(resolve(sourceRoot, 'scripts', name), '// build input\n')
  }
  const dependencies = ['rusqlite-0.39.0', 'i18n-embed-fl-0.9.4']
  for (const dependency of dependencies) {
    mkdirSync(resolve(sourceRoot, 'upstream/vendor', dependency, 'src'), { recursive: true })
    writeFileSync(resolve(sourceRoot, 'upstream/vendor', dependency, 'src/lib.rs'), 'pub const VERSION: u32 = 1;\n')
  }
  const git = (...args) => execFileSync('git', ['-C', sourceRoot, ...args], { encoding: 'utf8' }).trim()
  git('init', '--quiet')
  git('config', 'core.filemode', 'true')
  git('add', '.')
  git('-c', 'user.name=Source Seal Test', '-c', 'user.email=source-seal@example.invalid',
    '-c', 'commit.gpgSign=false', 'commit', '--quiet', '-m', 'fixture')
  const head = git('rev-parse', 'HEAD')
  for (const name of ['winwincode-server', 'winwincode-kernel-helper', 'winwincode-worker', 'wwc']) {
    const path = resolve(binaryRoot, name)
    writeFileSync(path, '#!/bin/sh\nexit 0\n')
    chmodSync(path, 0o755)
  }
  const serverBinary = resolve(binaryRoot, 'winwincode-server')
  const helperExecutable = resolve(binaryRoot, 'winwincode-kernel-helper')
  const options = { root: sourceRoot, serverBinary, helperExecutable }
  const sealed = writeApiProductionSourceSeal({ ...options,
    helperReleaseManifest: writeHelperReleaseManifest(sourceRoot, helperExecutable) })
  const originalDigest = projectSourceDigest(sourceRoot)
  verifyApiProductionSourceSeal(options)
  for (const dependency of dependencies) {
    const path = resolve(sourceRoot, 'upstream/vendor', dependency, 'src/lib.rs')
    writeFileSync(path, 'pub const VERSION: u32 = 2;\n')
    assert.equal(git('rev-parse', 'HEAD'), head)
    assert.notEqual(projectSourceDigest(sourceRoot), originalDigest,
      `${dependency}: patched dependency is a Rust build input`)
    assert.throws(() => verifyApiProductionSourceSeal(options), /source seal is stale/u)
    writeFileSync(path, 'pub const VERSION: u32 = 1;\n')
    verifyApiProductionSourceSeal(options)
    // Equal bytes with a changed tracked file mode still invalidate the sealed input.
    chmodSync(path, 0o755)
    assert.equal(projectSourceDigest(sourceRoot), originalDigest)
    assert.throws(() => verifyApiProductionSourceSeal(options), /tracked diff is stale/u)
    chmodSync(path, 0o644)
    assert.equal(verifyApiProductionSourceSeal(options).seal.gitHead, sealed.seal.gitHead)
  }
})

test('Device prerequisite truthfulness rejects accepted chats and unrelated error codes', async () => {
  const api = result => ({ async command(command) {
    if (command === 'session.create') return { outcome: 'completed', currentRevision: 1 }
    if (result === 'accepted') return { outcome: 'completed' }
    throw Object.assign(new Error('public probe error'), { code: result, status: 409 })
  } })
  for (const code of ['DEVICE_SESSION_REQUIRED', 'DEVICE_MODEL_UNAVAILABLE']) {
    const probe = await probeDeviceErrorTruthfulness(api(code))
    assert.equal(probe.truthfulDeviceCodes, true)
    assert.equal(probe.notWrongStateDisguise, true)
    assert.equal(probe.chatSubmit.accepted, false)
    assert.equal(probe.chatSubmit.code, code)
  }
  for (const code of ['WRONG_STATE', 'INTERNAL', 'accepted']) {
    await assert.rejects(probeDeviceErrorTruthfulness(api(code)),
      /Device prerequisite probe must reject with a dedicated Device error/u)
  }
  await assert.rejects(probeDeviceErrorTruthfulness({ async command() {
    throw Object.assign(new Error('public probe error'), { code: 'WRONG_STATE' })
  } }), /Device prerequisite probe must reject with a dedicated Device error/u)
})

test('Device cleanup records the final Provider request count and preserves its failure', async () => {
  for (const fails of [false, true]) {
    const requests = []
    const report = { devicePath: { modelServerRequestCount: 0 } }
    const failure = new Error('Device cleanup failed')
    const devicePath = { modelServer: { requests }, async stop() {
      requests.push({ request: 'first' }, { request: 'during shutdown' })
      if (fails) throw failure
    } }
    if (fails) await assert.rejects(stopDevicePath(report, devicePath), error => error === failure)
    else await stopDevicePath(report, devicePath)
    assert.equal(report.devicePath.modelServerRequestCount, 2)
  }
})
