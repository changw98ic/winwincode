import assert from 'node:assert/strict'
import test from 'node:test'
import { existsSync, globSync, mkdtempSync, readFileSync, rmSync, writeFileSync, mkdirSync, statSync } from 'node:fs'
import { createHash, randomBytes } from 'node:crypto'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { isDeepStrictEqual } from 'node:util'
import { crc32, inflateRawSync } from 'node:zlib'

import { driveDelivery, runApiProductionVertical, waitForDeviceWorkerRegistered,
  workItemCreatePayload } from '../scripts/run-api-production-vertical.mjs'
import { readDeviceExecutionReceipts } from '../scripts/export-device-candidate.mjs'
import { runDeviceTaskVertical } from '../scripts/run-device-task-vertical.mjs'
import { deviceTaskIdentities, withDeviceTaskRuntime } from '../scripts/device-task-runtime.mjs'
import { benchmarkConfiguration } from '../scripts/run-real-task-benchmark.mjs'
import { deterministicDeviceProvider, installDevicePublicSmoke, waitFor } from '../scripts/device-production-fixture.mjs'
import { cancellationEvidence } from './fixtures/code-mode-cancellation-evidence.mjs'
import { assertManagedProviderWorker, assertProviderRuntimeBoundary } from './fixtures/provider-secret-boundary.mjs'
import { assertSuccessfulRetainedCommands, completedCommandScript, loseCommittedCommandResponse,
  readRetainedCoreCommandResults } from './fixtures/chat-execution-transport-probe.mjs'

test('a scenario propagates its Device cleanup failure', async () => {
  await assert.rejects(runApiProductionVertical({
    devicePrerequisites: true,
    restart: false,
    repeat: false,
    scenario: { async run({ devicePath }) {
      const stop = devicePath.stop
      devicePath.stop = async () => {
        await stop()
        throw new Error('scenario cleanup probe')
      }
      return { completed: true }
    } },
  }), /scenario cleanup probe/u)
})

test('standalone Server API drives Chat and StrongFlow through Device Worker production vertical', async t => {
  const fixture = createChatScenario(t, 'wwc-standalone-production-')
  const providerKey = randomBytes(32).toString('hex')
  const privateHeader = randomBytes(32).toString('hex')
  const report = await runApiProductionVertical({
    directory: fixture.directory,
    devicePrerequisites: true,
    deviceProviderSecrets: [providerKey, privateHeader],
    deviceProvider: { ...deterministicDeviceProvider(), customHeaders: { 'x-device-private': privateHeader } },
  })
  assert.equal(report.schemaVersion, 'winwincode.api-production-vertical.v1')
  assert.equal(report.execution, 'device-worker-only')
  assert.ok(Array.isArray(report.devicePrerequisites))
  assert.ok(report.devicePrerequisites.includes('device-enroll-pair'))
  assert.ok(report.devicePrerequisites.includes('chat-strongflow-cancel-restart'))
  assert.equal(report.deviceProvider?.secretPlacement, 'device-local-only')
  assert.ok(report.devicePath, 'production vertical must expose Device path evidence')
  assert.ok(report.devicePath.publicClientId)
  assert.equal(report.flow.chat.status, 'Completed')
  assert.equal(report.flow.chat.assistant.role, 'assistant')
  assert.equal(report.flow.chat.assistant.state, 'completed')
  assert.ok(report.flow.chat.assistant.content.trim().length > 0)
  assert.equal(report.flow.strongflow.status, 'done')
  assert.equal(report.flow.strongflow.verdictStatus, 'pass')
  assert.ok(report.flow.strongflow.workItemStates.length > 0)
  assert.equal(report.flow.strongflow.workItemStates.every(state => state === 'done'), true)
  assert.ok(report.flow.strongflow.workRunStates.length > 0)
  assert.equal(report.flow.strongflow.workRunStates.every(state => state === 'settled'), true)
  const calls = report.flow.strongflow.workRunRuntime.flatMap(run => run.coreToolCalls)
  assert.ok(calls.some(call => call.toolName.endsWith('exec_command')
    && call.cellId !== null && call.parentRequestSequence !== null
    && call.execution === 'completed' && call.disposition === 'accepted'
    && call.delivery === 'offered'), 'public API must preserve the original nested Core command receipt')

  assert.deepEqual(report.deterministic, {
    contentEqual: true,
    firstSessionId: 'psn_01J00000000000000000000001',
    repeatSessionId: 'psn_01J00000000000000000000002',
  })
  assert.deepEqual(report.restart, {
    deliveryBytesStable: true,
    messageBytesStable: true,
    status: 'done',
  })
  const boundary = assertProviderRuntimeBoundary({ directory: fixture.directory, secrets: [providerKey, privateHeader] })
  assert.ok(boundary.files > boundary.configurations)
  fixture.complete()
})

function createChatScenario(t, prefix) {
  const directory = mkdtempSync(join(tmpdir(), prefix))
  let completed = false
  t.after(() => {
    if (completed) rmSync(directory, { recursive: true, force: true })
    else t.diagnostic(`Failed Chat production scenario retained at ${directory}`)
  })
  return { directory, complete() { completed = true } }
}

function apiProductionScenarioBinaries() {
  const installed = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  return installed === undefined ? {} : {
    build: false,
    serverBinary: join(installed, 'winwincode-server'),
    wwcBinary: join(installed, 'wwc'),
  }
}

async function assertUnknownExecutionAccounting(database, jobId, knownTokens) {
  const reconciliation = await waitFor(() => database.prepare(`SELECT * FROM execution_admission_reconciliation
    WHERE job_id=?`).get(jobId), 'the original terminal retains durable unknown accounting', 30_000)
  const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(jobId)
  assert.equal(reservation.state, 'released', 'an unmeasured physical Provider attempt cannot settle financial totals')
  assert.equal(reservation.release_reason, 'failed')
  assert.equal(reservation.actual_tokens, null)
  assert.equal(reservation.actual_cost_microunits, null)
  assert.equal(reconciliation.accounting_status, 'pending')
  assert.equal(reconciliation.settlement_request_id, null)
  assert.equal(reconciliation.settlement_usage_json, null)
  const terminalUsage = JSON.parse(reconciliation.terminal_usage_json)
  const latest = database.prepare(`SELECT usage_json FROM execution_admission_reconciliation_facts
    WHERE job_id=? ORDER BY sequence DESC LIMIT 1`).get(jobId)
  for (const usage of [terminalUsage, latest ? JSON.parse(latest.usage_json) : terminalUsage]) {
    assert.equal(usage.accountingStatus, 'unknown')
    assert.equal(usage.tokens ?? null, null)
    assert.equal(usage.costMicrounits ?? null, null)
    assert.equal(usage.knownTokens, knownTokens, 'observed successful usage remains a lower bound, not a fabricated total')
    assert.ok(Number.isSafeInteger(usage.runtimeMillis) && usage.runtimeMillis >= 0)
  }
  return { reservation: reservation.state, accounting: reconciliation.accounting_status,
    knownTokens: terminalUsage.knownTokens }
}

test('Chat replay preserves one execution and its durable terminal and accounting identities', async t => {
  const fixture = createChatScenario(t, 'wwc-chat-execution-')
  const { directory } = fixture
  const productSessionId = 'psn_01J00000000000000000000021'
  const requestId = 'req_01J00000000000000000000021'
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...apiProductionScenarioBinaries(),
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: {
      'native-cell-A': 'const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command")); if(!command) throw new Error("command tool missing"); text(await tools[command.name]({cmd:"git rev-parse --verify HEAD",yield_time_ms:1000}));',
    } },
    scenario: { async run({ api, modelRoute, devicePath }) {
      await api.command('session.create', 0, {
        productSessionId, projectId: 'prj_01J00000000000000000000000',
        repositoryId: 'rep_01J00000000000000000000000', title: 'Chat execution identity', modelRoute,
      })
      await devicePath.launchAnchor({ productSessionId })
      const payload = { productSessionId, message: 'native-cell-A' }
      const lost = await loseCommittedCommandResponse(api, 'chat.submit', 1, payload, requestId)
      assert.ok(lost.status >= 200 && lost.status < 300)
      assert.equal(lost.bodyComplete, false, 'the first response body was interrupted')
      assert.equal(lost.destroyed, true, 'the first command connection was actually severed')
      const submitted = await api.command('chat.submit', 1, payload, requestId)
      assert.equal(submitted.outcome, 'completed')
      const replay = await api.command('chat.submit', 1, payload, requestId)
      assert.deepEqual(replay.result, submitted.result, 'a lost submit response replays the original receipt')
      await assert.rejects(api.command('chat.submit', 1, {
        ...payload, message: 'Changed input with the original request identity',
      }, requestId), error => error.status === 409)
      await assert.rejects(api.command('chat.submit', 1, payload), error => error.status === 409)
      const database = new DatabaseSync(join(directory, 'server-data', 'control-plane.sqlite3'), { readOnly: true })
      try {
        database.exec('PRAGMA busy_timeout=5000')
        const jobs = () => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE product_session_id = ?')
          .all(productSessionId)
        const [job] = jobs()
        assert.equal(jobs().length, 1)
        assert.equal(job.submission_request_id, requestId)
        assert.equal(job.attempt, 1)
        assert.equal(job.work_run_id, null, 'ordinary Chat keeps its ProductSession scope')
        const runtime = await waitFor(async () => {
          const session = (await api.query('runtime.projection.get', {
            kind: 'product-session', productSessionId,
          })).result.sessions.find(row => row.productSessionId === productSessionId)
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          const current = jobs()[0]
          const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id = ?').get(job.job_id)
          const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id = ?').get(job.job_id)
          return session && messages.some(row => row.role === 'assistant' && row.state === 'completed')
            && current.state === 'completed' && terminal?.outcome === 'completed' && reservation?.state === 'settled'
            ? { session, messages, terminal, reservation } : false
        }, 'Chat projection and durable execution resources settled', 120_000)
        assert.equal(runtime.session.executionJobId, job.job_id)
        assert.ok(runtime.session.activities.some(row => row.coreTool?.call?.toolName.endsWith('exec_command')
          && row.coreTool.call.cellId !== null && row.coreTool.call.parentRequestSequence !== null
          && row.coreTool.call.execution === 'completed' && row.coreTool.call.disposition === 'accepted'
          && row.coreTool.call.delivery === 'offered'), 'the real Core command receipt is projected by this execution')
        const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id = ?').get(job.job_id)
        assert.equal(authority.worker_session_id, runtime.session.workerSessionId)
        for (const key of ['job_id', 'attempt', 'worker_id', 'worker_instance_id', 'fencing_token', 'lease_id']) {
          assert.equal(runtime.terminal[key], authority[key], `terminal preserves dispatch ${key}`)
        }
        const receipts = readDeviceExecutionReceipts(directory)
        assert.ok(receipts.calls.length > 0, 'real Device Provider calls must be retained')
        assert.ok(receipts.calls.every(call => call.jobId === job.job_id
          && call.workerSessionId === runtime.session.workerSessionId
          && call.providerAttempts.length === 1 && call.providerAttempts[0].state === 'completed'))
        const requestCount = devicePath.modelServer.requests.length
        const terminalReplay = await api.command('chat.submit', 1, payload, requestId)
        assert.deepEqual(terminalReplay.result, submitted.result)
        assert.equal(jobs().length, 1)
        assert.equal(devicePath.modelServer.requests.length, requestCount, 'terminal replay emits no new HTTP call')
        assert.equal(runtime.messages.filter(row => row.role === 'user').length, 1)
        assert.equal((await api.query('session.get', { productSessionId })).result.state, 'idle')
        return { jobId: job.job_id, attempt: job.attempt, workerSessionId: runtime.session.workerSessionId,
          codexThreadId: runtime.session.codexThreadId, modelCalls: receipts.calls.length,
          terminal: runtime.terminal.outcome, reservation: runtime.reservation.state }
      } finally { database.close() }
    } },
  })
  assert.equal(report.flow.scenario.terminal, 'completed')
  assert.equal(report.flow.scenario.reservation, 'settled')
  fixture.complete()
})

test('Chat Provider retry and exhaustion preserve physical accounting and one execution terminal', async t => {
  const fixture = createChatScenario(t, 'wwc-chat-provider-retry-')
  const { directory } = fixture
  const providerKey = randomBytes(32).toString('hex')
  const privateHeader = randomBytes(32).toString('hex')
  const productSessionId = 'psn_01J00000000000000000000022'
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...apiProductionScenarioBinaries(),
    deviceProviderSecrets: [providerKey, privateHeader],
    deviceProvider: { ...deterministicDeviceProvider(), customHeaders: { 'x-device-private': privateHeader }, httpFailures: {
      'retry-final-failure': { status: 429, count: 100 },
      'retry-once': { status: 503, count: 1 },
    } },
    scenario: { async run({ api, modelRoute, devicePath }) {
      await api.command('session.create', 0, {
        productSessionId, projectId: 'prj_01J00000000000000000000000',
        repositoryId: 'rep_01J00000000000000000000000', title: 'Chat Provider retry', modelRoute,
      })
      const launched = await devicePath.launchAnchor({ productSessionId })
      await assertManagedProviderWorker({ deviceData: devicePath.deviceData, launched, secrets: [providerKey, privateHeader] })
      const database = new DatabaseSync(join(directory, 'server-data', 'control-plane.sqlite3'), { readOnly: true })
      try {
        database.exec('PRAGMA busy_timeout=5000')
        const jobs = () => database.prepare('SELECT * FROM scheduler_execution_jobs WHERE product_session_id = ? ORDER BY submitted_at')
          .all(productSessionId)
        const run = async (message, terminalState) => {
          const session = (await api.query('session.get', { productSessionId })).result
          const requestId = message === 'retry-once' ? 'req_01J00000000000000000000022' : 'req_01J00000000000000000000023'
          const firstRequest = devicePath.modelServer.requests.length
          await api.command('chat.submit', session.revision, { productSessionId, message }, requestId)
          return waitFor(async () => {
            const job = jobs().find(row => row.submission_request_id === requestId)
            const terminal = job && database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id = ?').get(job.job_id)
            const messages = (await api.query('session.messages.list', { productSessionId })).result.items
            const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job?.job_id)
            return job?.state === terminalState && terminal?.outcome === terminalState
              && messages.some(row => row.role === 'assistant' && row.state === terminalState)
              && calls.length > 0
              ? { job, terminal, calls, http: devicePath.modelServer.requests.slice(firstRequest) } : false
          }, `Chat ${message} terminal and accounting`, 120_000)
        }
        const retried = await run('retry-once', 'completed')
        assert.equal(retried.job.attempt, 1, 'a Provider retry does not replace the execution attempt')
        assert.equal(retried.calls.length, 1)
        assert.deepEqual(retried.http.map(request => request.status), [503, 200])
        assert.deepEqual(retried.calls[0].providerAttempts.map(attempt => attempt.state), ['failed', 'completed'])
        assert.equal(retried.calls[0].terminalType, 'completed')
        assert.equal(retried.calls[0].providerAttempts[0].usage, null)
        await assertUnknownExecutionAccounting(database, retried.job.job_id,
          retried.calls[0].providerAttempts[1].usage.totalTokens)
        const exhausted = await run('retry-final-failure', 'failed')
        assert.equal(exhausted.job.attempt, 1)
        assert.equal(exhausted.calls.length, 1, 'Core must consume the host terminal without a second retry policy')
        assert.deepEqual(exhausted.http.map(request => request.status), [429, 429, 429, 429])
        assert.deepEqual(exhausted.calls[0].providerAttempts.map(attempt => attempt.state), ['failed', 'failed', 'failed', 'failed'])
        assert.equal(exhausted.calls[0].failure.code, 'DEVICE_PROVIDER_RATE_LIMITED')
        assert.equal(exhausted.calls[0].failure.retryable, false)
        assert.equal(exhausted.calls[0].failure.status, 429)
        assert.ok(exhausted.calls[0].providerAttempts.every(attempt => attempt.usage === null))
        await assertUnknownExecutionAccounting(database, exhausted.job.job_id, 0)
        assert.equal(jobs().length, 2)
        assert.equal(database.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id IN (?, ?)')
          .get(retried.job.job_id, exhausted.job.job_id).count, 2)
        return { retried: retried.job.job_id, exhausted: exhausted.job.job_id }
      } finally { database.close() }
    } },
  })
  assert.ok(report.flow.scenario.retried)
  assert.ok(report.flow.scenario.exhausted)
  assertProviderRuntimeBoundary({ directory, secrets: [providerKey, privateHeader] })
  fixture.complete()
})

function deviceProviderDatabases(deviceData, options = {}) {
  const paths = globSync('**/provider-runtime.sqlite3', { cwd: deviceData }).map(path => join(deviceData, path))
  assert.ok(paths.length > 0, 'the real Device Provider store must already exist')
  return paths.map(path => {
    const database = new DatabaseSync(path, options)
    database.exec('PRAGMA busy_timeout=5000')
    return database
  })
}

function retainedCanonicalModelFailures(providers, jobId) {
  return providers.flatMap(database => database.prepare(`SELECT e.request_open, a.attempt_number,
    a.accounting_chunks FROM model_invocation_attempts a JOIN exchanges e ON e.exchange_id=a.exchange_id
    WHERE a.accounting_chunks IS NOT NULL ORDER BY a.attempt_number`).all().flatMap(row => {
    const open = JSON.parse(row.request_open)
    if (open.lease.jobId !== jobId) return []
    return JSON.parse(row.accounting_chunks).flatMap(chunk => {
      if (!chunk.payload) return []
      const frame = JSON.parse(Buffer.from(chunk.payload.dataBase64, 'base64').toString('utf8'))
      return frame.type === 'error' ? [{ open, chunk, frame, attempt: row.attempt_number }] : []
    })
  }))
}

test('Chat consumes canonical SSE errors inside HTTP 200 through one Provider retry policy and one execution terminal', async t => {
  const fixture = createChatScenario(t, 'wwc-chat-canonical-error-')
  const { directory } = fixture
  const cases = [
    { marker: 'canonical-overload', type: 'overloaded_error', count: 1, physical: 2,
      code: 'SERVER', state: 'completed', sessionId: 'psn_01J00000000000000000000051' },
    { marker: 'canonical-auth', type: 'authentication_error', count: 100, physical: 1,
      code: 'AUTH', state: 'failed', sessionId: 'psn_01J00000000000000000000052' },
    { marker: 'canonical-invalid', type: 'invalid_request_error', count: 100, physical: 1,
      code: 'INVALID_REQUEST', state: 'failed', sessionId: 'psn_01J00000000000000000000053' },
    { marker: 'canonical-rate', type: 'rate_limit_error', count: 100, physical: 4,
      code: 'RATE_LIMIT', state: 'failed', sessionId: 'psn_01J00000000000000000000054' },
  ]
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...apiProductionScenarioBinaries(),
    deviceProvider: { ...deterministicDeviceProvider(), streamFailures: Object.fromEntries(
      cases.map(item => [item.marker, { type: item.type, count: item.count }])) },
    scenario: { async run(context) {
      const { api, devicePath } = context
      const database = new DatabaseSync(join(directory, 'server-data', 'control-plane.sqlite3'), { readOnly: true })
      database.exec('PRAGMA busy_timeout=5000')
      const providers = []
      try {
        const results = []
        for (const item of cases) {
          await createAnchoredChat(context, item.sessionId, `Canonical SSE ${item.type}`)
          const firstRequest = devicePath.modelServer.requests.length
          await api.command('chat.submit', 1, { productSessionId: item.sessionId, message: item.marker })
          const settled = await waitFor(async () => {
            const jobs = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE product_session_id=?').all(item.sessionId)
            const [job] = jobs
            const terminal = job && database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(job.job_id)
            const reservation = job && database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(job.job_id)
            const messages = (await api.query('session.messages.list', { productSessionId: item.sessionId })).result.items
            const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job?.job_id)
            return jobs.length === 1 && job.state === item.state && terminal?.outcome === item.state
              && reservation?.terminal_at && calls.length === 1
              && messages.some(message => message.role === 'assistant' && message.state === item.state)
              ? { job, terminal, call: calls[0], messages } : false
          }, `${item.type} settles the original execution and physical Provider receipts`, 120_000)
          if (providers.length === 0) providers.push(...deviceProviderDatabases(devicePath.deviceData, { readOnly: true }))
          const http = devicePath.modelServer.requests.slice(firstRequest)
          assert.equal(http.length, item.physical)
          assert.ok(http.every(request => request.status === 200), 'canonical errors arrive inside successful HTTP responses')
          assert.equal(http.filter(request => request.streamErrorType === item.type).length,
            item.state === 'completed' ? 1 : item.physical)
          assert.equal(settled.job.attempt, 1, 'Provider retries preserve the execution attempt')
          assert.equal(settled.call.providerAttempts.length, item.physical,
            'each real HTTPS invocation has one durable physical attempt')
          assert.deepEqual(settled.call.providerAttempts.map(attempt => attempt.state),
            item.state === 'completed' ? ['failed', 'completed'] : Array(item.physical).fill('failed'))
          assert.ok(settled.call.providerAttempts.filter(attempt => attempt.state === 'failed')
            .every(attempt => attempt.usage === null), 'error-only SSE carries no invented physical usage')
          const knownTokens = settled.call.providerAttempts.reduce((total, attempt) => total + (attempt.usage?.totalTokens ?? 0), 0)
          const accounting = await assertUnknownExecutionAccounting(database, settled.job.job_id, knownTokens)
          const failures = retainedCanonicalModelFailures(providers, settled.job.job_id)
          assert.equal(failures.length, item.state === 'completed' ? 1 : item.physical)
          for (const failure of failures) {
            assert.equal(failure.chunk.isFinal, true)
            assert.equal(failure.chunk.error ?? null, null, 'the canonical payload is authoritative even when the outer error is empty')
            assert.equal(failure.frame.error.code, item.code)
            assert.equal(failure.open.lease.jobId, settled.job.job_id)
            assert.equal(failure.chunk.modelExchangeId, settled.call.exchangeId)
          }
          assert.equal(settled.call.terminalType, item.state === 'completed' ? 'completed' : 'error')
          if (item.state === 'failed') {
            assert.equal(settled.call.failure.code, item.code === 'RATE_LIMIT' ? 'DEVICE_PROVIDER_RATE_LIMITED' : item.code)
            assert.equal(settled.call.failure.retryable, false)
          }
          assert.equal(database.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id=?')
            .get(settled.job.job_id).count, 1)
          assert.equal(settled.messages.filter(message => message.role === 'assistant'
            && ['completed', 'failed', 'cancelled'].includes(message.state)).length, 1)
          results.push({ type: item.type, canonicalCode: item.code, jobId: settled.job.job_id,
            physicalAttempts: http.length, terminal: settled.terminal.outcome, ...accounting })
        }
        return { cases: results }
      } finally { for (const provider of providers) provider.close(); database.close() }
    } },
  })
  assert.equal(report.flow.scenario.cases.length, 4)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  fixture.complete()
})

async function createAnchoredChat({ api, modelRoute, devicePath }, productSessionId, title) {
  await api.command('session.create', 0, {
    productSessionId, projectId: 'prj_01J00000000000000000000000',
    repositoryId: 'rep_01J00000000000000000000000', title, modelRoute,
  })
  return devicePath.launchAnchor({ productSessionId })
}

test('Chat cancellation fences a durable Provider backoff before another physical HTTP attempt', async t => {
  const fixture = createChatScenario(t, 'wwc-chat-backoff-cancel-')
  const { directory } = fixture
  const productSessionId = 'psn_01J00000000000000000000031'
  const requestId = 'req_01J00000000000000000000031'
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...apiProductionScenarioBinaries(),
    deviceProvider: { ...deterministicDeviceProvider(), httpFailures: {
      'cancel-provider-backoff': { status: 429, count: 100, retryAfterSeconds: 10 },
    } },
    scenario: { async run(context) {
      const { api, devicePath } = context
      await createAnchoredChat(context, productSessionId, 'Cancel durable Provider backoff')
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
      const providers = deviceProviderDatabases(devicePath.deviceData, { readOnly: true })
      try {
        await api.command('chat.submit', 1, { productSessionId, message: 'cancel-provider-backoff' }, requestId)
        const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id = ?').get(requestId)
        assert.ok(job, 'the public submit must retain its original Job')
        const retry = await waitFor(() => providers.flatMap(store => store.prepare(`
          SELECT r.exchange_id, r.policy_attempt, r.wake_ms, r.decision, e.request_open
          FROM model_retry_state r JOIN exchanges e ON e.exchange_id = r.exchange_id`).all())
          .find(row => JSON.parse(row.request_open).lease.jobId === job.job_id
            && row.wake_ms > Date.now() + 1000), 'durable Provider backoff under the original Chat Job', 30_000)
        assert.equal(devicePath.modelServer.requests.length, 1)
        assert.equal(devicePath.modelServer.requests[0].status, 429)
        const current = (await api.query('session.get', { productSessionId })).result
        const cancelPayload = { productSessionId, reason: 'Cancel the observed durable Provider backoff' }
        const cancelRequest = 'req_01J00000000000000000000032'
        const cancelled = await api.command('session.cancel', current.revision, cancelPayload, cancelRequest)
        assert.equal(cancelled.result.state, 'cancelled')
        const settled = await waitFor(async () => {
          const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id = ?').get(job.job_id)
          const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id = ?').get(job.job_id)
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          return terminal?.outcome === 'cancelled' && reservation?.state === 'released'
            && messages.some(message => message.role === 'assistant' && message.state === 'cancelled')
            ? { terminal, reservation } : false
        }, 'cancellation terminal and real execution cleanup after Provider backoff', 60_000)
        assert.equal(settled.reservation.release_reason, 'cancelled')
        await waitFor(() => Date.now() >= retry.wake_ms + 500,
          'the original Provider retry deadline has passed', 30_000)
        assert.equal(devicePath.modelServer.requests.length, 1, 'a fenced durable retry cannot start another HTTP attempt')
        const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id)
        assert.equal(calls.length, 1)
        assert.equal(calls[0].providerAttempts.length, 1)
        assert.equal(calls[0].providerAttempts[0].state, 'failed')
        const cancellationReplay = await api.command('session.cancel', current.revision, cancelPayload, cancelRequest)
        assert.deepEqual(cancellationReplay.result, cancelled.result)
        assert.equal(database.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id = ?').get(job.job_id).count, 1)
        assert.equal(devicePath.modelServer.requests.length, 1)
        return { jobId: job.job_id, modelExchangeId: retry.exchange_id,
          physicalAttempts: calls[0].providerAttempts.length, terminal: settled.terminal.outcome,
          reservation: settled.reservation.state }
      } finally { for (const store of providers) store.close(); database.close() }
    } },
  })
  assert.equal(report.flow.scenario.terminal, 'cancelled')
  t.diagnostic(JSON.stringify(report.flow.scenario))
  fixture.complete()
})

test('Chat auxiliary Provider diagnostics can fail while real Core execution and physical accounting complete', async t => {
  const fixture = createChatScenario(t, 'wwc-chat-aux-failure-')
  const { directory } = fixture
  const productSessionId = 'psn_01J00000000000000000000033'
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...apiProductionScenarioBinaries(),
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: {
      'native-cell-A': 'const command=ALL_TOOLS.find(tool=>tool.name.endsWith("exec_command")); if(!command) throw new Error("command tool missing"); text(await tools[command.name]({cmd:"git rev-parse --verify HEAD",yield_time_ms:1000}));',
    } },
    scenario: { async run(context) {
      const { api, devicePath } = context
      await createAnchoredChat(context, productSessionId, 'Auxiliary diagnostics failure')
      const providers = deviceProviderDatabases(devicePath.deviceData)
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
      try {
        for (const store of providers) store.exec(`CREATE TRIGGER fail_optional_model_diagnostic
          BEFORE INSERT ON model_attempt_diagnostics
          BEGIN SELECT RAISE(FAIL, 'injected auxiliary diagnostic failure'); END`)
        const requestId = 'req_01J00000000000000000000033'
        await api.command('chat.submit', 1, { productSessionId, message: 'native-cell-A' }, requestId)
        const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id = ?').get(requestId)
        assert.ok(job)
        const completed = await waitFor(async () => {
          const current = database.prepare('SELECT state FROM scheduler_execution_jobs WHERE job_id = ?').get(job.job_id)
          const terminal = database.prepare('SELECT outcome FROM execution_lease_terminals WHERE job_id = ?').get(job.job_id)
          const reservation = database.prepare('SELECT state FROM execution_admission_reservations WHERE job_id = ?').get(job.job_id)
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          const runtime = (await api.query('runtime.projection.get', { kind: 'product-session', productSessionId })).result.sessions
            .find(session => session.productSessionId === productSessionId)
          return current?.state === 'completed' && terminal?.outcome === 'completed' && reservation?.state === 'settled'
            && messages.some(message => message.role === 'assistant' && message.state === 'completed')
            && runtime?.activities.some(row => row.coreTool?.call?.toolName.endsWith('exec_command')
              && row.coreTool.call.execution === 'completed' && row.coreTool.call.disposition === 'accepted')
            ? { runtime, terminal } : false
        }, 'real Core command completes despite optional Provider diagnostic storage failure', 120_000)
        const calls = readDeviceExecutionReceipts(directory).calls.filter(call => call.jobId === job.job_id)
        assert.ok(calls.length >= 2, 'the command and its final reply use the real Provider')
        assert.ok(calls.every(call => call.providerAttempts.length === 1
          && call.providerAttempts[0].state === 'completed' && call.providerAttempts[0].usage !== null))
        assert.ok(devicePath.modelServer.requests.every(request => request.status === 200))
        assert.equal(providers.reduce((count, store) => count + store.prepare('SELECT COUNT(*) AS count FROM model_attempt_diagnostics').get().count, 0), 0)
        return { jobId: job.job_id, workerSessionId: completed.runtime.workerSessionId,
          modelCalls: calls.length, physicalAttempts: calls.reduce((count, call) => count + call.providerAttempts.length, 0),
          terminal: completed.terminal.outcome }
      } finally {
        for (const store of providers) { store.exec('DROP TRIGGER IF EXISTS fail_optional_model_diagnostic'); store.close() }
        database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.terminal, 'completed')
  t.diagnostic(JSON.stringify(report.flow.scenario))
  fixture.complete()
})

test('Chat project output preserves frozen ZIP bytes and exposes preparation before its required Artifact ACK', async t => {
  const fixture = createChatScenario(t, 'wwc-chat-project-output-')
  const { directory } = fixture
  const productSessionId = 'psn_01J00000000000000000000034'
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...apiProductionScenarioBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: {
      'native-cell-A': completedCommandScript("printf '%s\\n' 'Chat project output' > CHAT-OUTPUT.txt"),
    } },
    scenario: { async run(context) {
      const { api, devicePath } = context
      await createAnchoredChat(context, productSessionId, 'Frozen Chat project output')
      const catalog = new DatabaseSync(join(directory, 'server-data/artifact-catalog/artifact-catalog.sqlite3'))
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
      try {
        catalog.exec(`CREATE TRIGGER delay_chat_project_chunk BEFORE INSERT ON artifact_chunks
          WHEN EXISTS(SELECT 1 FROM artifacts WHERE artifact_id=NEW.artifact_id
            AND kind='candidate' AND media_type='application/zip' AND file_name='project.zip')
          BEGIN SELECT RAISE(FAIL, 'injected project upload delay'); END`)
        const requestId = 'req_01J00000000000000000000034'
        await api.command('chat.submit', 1, { productSessionId, message: 'native-cell-A' }, requestId)
        const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id = ?').get(requestId)
        assert.ok(job)
        const preparing = await waitFor(async () => {
          const artifact = catalog.prepare(`SELECT * FROM artifacts WHERE execution_job_id = ?
            AND kind='candidate' AND media_type='application/zip' AND file_name='project.zip'`).get(job.job_id)
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          const assistant = messages.find(message => message.role === 'assistant' && message.outputPreparation === 'preparing')
          return artifact && assistant ? { artifact, assistant } : false
        }, 'accepted project ArtifactOpen derives preparation during the actual upload delay', 120_000)
        assert.equal(preparing.artifact.complete, 0)
        assert.equal(preparing.artifact.acknowledged_sequence, 0)
        assert.equal(preparing.assistant.state, 'streaming')
        assert.equal(preparing.assistant.artifactRefs ?? null, null, 'an unfinished upload is not downloadable')
        assert.equal(database.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id = ?').get(job.job_id).count, 0)
        await assert.rejects(api.query('session.artifact.get', {
          productSessionId, artifactId: preparing.artifact.artifact_id, offset: 0, length: 262144,
        }), 'the public API cannot download an unfinished project')
        const frozen = globSync('**/worker-codex.sqlite3', { cwd: devicePath.deviceData }).flatMap(path => {
          const store = new DatabaseSync(join(devicePath.deviceData, path), { readOnly: true })
          try {
            if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='candidate_artifact_content'").get()) return []
            return store.prepare(`SELECT c.bytes FROM candidate_artifact_content c
              JOIN candidate_artifact_upload u ON u.authority_key = c.authority_key WHERE u.artifact_id = ?`)
              .all(preparing.artifact.artifact_id).map(row => Buffer.from(row.bytes))
          } finally { store.close() }
        })
        assert.equal(frozen.length, 1, 'one original frozen candidate is retained by the actual Adapter')
        const digest = `sha256:${createHash('sha256').update(frozen[0]).digest('hex')}`
        assert.equal(digest, preparing.artifact.digest)
        await waitFor(() => database.prepare(`SELECT 1 FROM execution_lease_request_receipts
          WHERE job_id=? AND operation='renew' AND json_extract(response_json,'$.status')='accepted'`)
          .get(job.job_id), 'the delayed original project upload crosses a real lease renewal', 30_000)
        catalog.exec('DROP TRIGGER delay_chat_project_chunk')
        const completed = await waitFor(async () => {
          const artifact = catalog.prepare('SELECT complete FROM artifacts WHERE artifact_id = ?').get(preparing.artifact.artifact_id)
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          const assistant = messages.find(message => message.id === preparing.assistant.id && message.state === 'completed')
          return artifact?.complete === 1 && assistant?.artifactRefs?.length === 1 ? assistant : false
        }, 'original candidate upload ACK allows the one Chat terminal and download', 120_000)
        assert.equal(completed.outputPreparation ?? null, null)
        assert.deepEqual(completed.artifactRefs, [{ artifactId: preparing.artifact.artifact_id, digest }])
        const downloaded = (await api.query('session.artifact.get', {
          productSessionId, artifactId: preparing.artifact.artifact_id, offset: 0, length: 262144,
        })).result
        assert.equal(downloaded.totalSize, frozen[0].length)
        assert.equal(downloaded.digest, digest)
        assert.deepEqual(Buffer.from(downloaded.dataBase64, 'base64'), frozen[0])
        assert.equal(database.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id = ?').get(job.job_id).count, 1)
        return { jobId: job.job_id, artifactId: preparing.artifact.artifact_id, digest, bytes: frozen[0].length,
          terminal: completed.state }
      } finally { catalog.exec('DROP TRIGGER IF EXISTS delay_chat_project_chunk'); catalog.close(); database.close() }
    } },
  })
  assert.equal(report.flow.scenario.terminal, 'completed')
  t.diagnostic(JSON.stringify(report.flow.scenario))
  fixture.complete()
})

function retainedInputCalls(deviceData) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='input_call'").get()) return []
      return store.prepare('SELECT record_json FROM input_call').all()
        .map(row => JSON.parse(Buffer.from(row.record_json).toString('utf8')))
    } finally { store.close() }
  })
}

function retainedCodexRuns(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_run'").get()) return []
      return store.prepare('SELECT record_json FROM codex_run').all()
        .map(row => JSON.parse(Buffer.from(row.record_json).toString('utf8')))
        .filter(record => record.job?.jobId === jobId)
    } finally { store.close() }
  })
}

function leaseRecord(authority) {
  return Object.fromEntries(['job_id', 'lease_id', 'payload_digest', 'worker_id', 'worker_instance_id',
    'attempt', 'fencing_token', 'issued_at', 'expires_at'].map(field => [field, authority[field]]))
}

function assertAcceptedLeasePeriod(database, period) {
  const receipts = database.prepare(`SELECT response_json FROM execution_lease_request_receipts
    WHERE job_id=? AND operation IN ('claim','renew') AND json_extract(response_json,'$.status')='accepted'
      AND json_extract(response_json,'$.lease.expires_at')=?`).all(period.job_id, period.expires_at)
  assert.ok(receipts.some(row => isDeepStrictEqual(JSON.parse(row.response_json).lease, period)),
    'the exact original lease period has a real accepted claim or renewal receipt')
}

function currentExecutionLease(database, authority, retained) {
  const current = database.prepare('SELECT * FROM execution_leases WHERE job_id=?').get(authority.job_id)
  if (!current) return false
  const original = leaseRecord(authority)
  for (const field of Object.keys(original).filter(field => field !== 'expires_at')) {
    assert.equal(current[field], original[field], `renewal preserves the original ${field}`)
  }
  assertAcceptedLeasePeriod(database, original)
  assertAcceptedLeasePeriod(database, leaseRecord(current))
  const period = { job_id: retained.jobId, lease_id: retained.leaseId, payload_digest: authority.payload_digest,
    worker_id: retained.workerId, worker_instance_id: retained.workerInstanceId, attempt: retained.attempt,
    fencing_token: retained.fencingToken, issued_at: retained.issuedAt, expires_at: retained.expiresAt }
  for (const field of Object.keys(period).filter(field => field !== 'expires_at')) assert.equal(period[field], original[field])
  assertAcceptedLeasePeriod(database, period)
  assert.ok(Date.parse(current.expires_at) >= Date.parse(period.expires_at))
  assert.deepEqual(database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(authority.job_id), authority,
    'heartbeat renewal leaves the accepted dispatch authority immutable')
  return current
}

function coreCommandAttempts(deviceData, jobId) {
  const runs = retainedCodexRuns(deviceData, jobId)
  assert.equal(runs.length, 1)
  const rows = globSync('**/state_5.sqlite', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      return store.prepare(`SELECT r.sequence,r.request_json,a.execution,a.disposition,a.effective_input,a.execution_result,a.accepted_result
        FROM tool_requests r JOIN tool_attempts a ON a.request_sequence=r.sequence WHERE r.thread_id=?`)
        .all(runs[0].kernelSessionId).filter(row => JSON.parse(row.request_json).tool_name.endsWith('exec_command'))
        .map(row => ({ ...row, command: JSON.parse(JSON.parse(row.effective_input).input).cmd }))
    } finally { store.close() }
  })
  return rows
}

function assertSuccessfulCoreCommand(deviceData, jobId, command) {
  const rows = coreCommandAttempts(deviceData, jobId).filter(row => row.command === command)
  assert.equal(rows.length, 1, 'the exact original Core shell command has one actual attempt')
  assert.equal(rows[0].execution, 'completed')
  assert.equal(rows[0].disposition, 'accepted')
  const [run] = retainedCodexRuns(deviceData, jobId)
  const retained = readRetainedCoreCommandResults(deviceData, run.kernelSessionId)
    .filter(row => row.sequence === rows[0].sequence)
  assertSuccessfulRetainedCommands(retained, 1)
}

function frozenZipFile(bytes, fileName) {
  const end = bytes.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]))
  assert.ok(end >= 0 && end + 22 <= bytes.length)
  assert.equal(end + 22 + bytes.readUInt16LE(end + 20), bytes.length)
  assert.equal(bytes.readUInt16LE(end + 4), 0)
  assert.equal(bytes.readUInt16LE(end + 6), 0)
  const count = bytes.readUInt16LE(end + 10)
  assert.equal(bytes.readUInt16LE(end + 8), count)
  let offset = bytes.readUInt32LE(end + 16)
  const entries = []
  for (let index = 0; index < count; index++) {
    assert.equal(bytes.readUInt32LE(offset), 0x02014b50)
    const length = bytes.readUInt16LE(offset + 28)
    const name = bytes.subarray(offset + 46, offset + 46 + length).toString('utf8')
    if (name === fileName) entries.push(offset)
    offset += 46 + length + bytes.readUInt16LE(offset + 30) + bytes.readUInt16LE(offset + 32)
  }
  assert.equal(offset, end)
  assert.equal(entries.length, 1, 'the frozen project ZIP contains one exact expected file')
  const entry = entries[0]
  assert.equal(bytes.readUInt16LE(entry + 8) & 1, 0)
  const method = bytes.readUInt16LE(entry + 10)
  assert.ok(method === 0 || method === 8)
  const local = bytes.readUInt32LE(entry + 42)
  assert.equal(bytes.readUInt32LE(local), 0x04034b50)
  const start = local + 30 + bytes.readUInt16LE(local + 26) + bytes.readUInt16LE(local + 28)
  const compressed = bytes.subarray(start, start + bytes.readUInt32LE(entry + 20))
  const result = method === 0 ? compressed : inflateRawSync(compressed, { maxOutputLength: 1024 * 1024 })
  assert.equal(result.length, bytes.readUInt32LE(entry + 24))
  assert.equal(crc32(result), bytes.readUInt32LE(entry + 16))
  return result
}

function assertFrozenChatFile(deviceData, jobId, fileName, content) {
  const candidates = globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='candidate_artifact_content'").get()) return []
      return store.prepare(`SELECT u.record_json,c.bytes FROM candidate_artifact_upload u
        JOIN candidate_artifact_content c ON c.authority_key=u.authority_key`).all()
        .map(row => ({ record: JSON.parse(Buffer.from(row.record_json).toString('utf8')), bytes: Buffer.from(row.bytes) }))
        .filter(value => value.record.open_message.lease.jobId === jobId)
    } finally { store.close() }
  })
  assert.equal(candidates.length, 1, 'the original Job retains one frozen candidate ZIP')
  const { record, bytes } = candidates[0]
  assert.equal(record.descriptor.fileName, 'project.zip')
  assert.equal(record.descriptor.mediaType, 'application/zip')
  assert.equal(record.descriptor.digest, `sha256:${createHash('sha256').update(bytes).digest('hex')}`)
  assert.equal(record.final_ack?.status, 'accepted')
  assert.equal(record.final_ack.artifactId, record.descriptor.artifactId)
  assert.equal(record.ack_sequence, record.chunk_count)
  assert.equal(record.final_ack.ackSequence, record.chunk_count)
  assert.deepEqual(frozenZipFile(bytes, fileName), Buffer.from(content))
}

function retainedInteractionRequests(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='execution_outbox'").get()) return []
      return store.prepare("SELECT frame_json FROM execution_outbox WHERE family IN ('input_request','approval_request')").all()
        .map(row => JSON.parse(Buffer.from(row.frame_json).toString('utf8')))
        .filter(frame => frame.lease.jobId === jobId)
    } finally { store.close() }
  })
}

test('Chat answers every real Core question once and retains sensitive values only in the private Adapter', async t => {
  const fixture = createChatScenario(t, 'wwc-chat-multiquestion-')
  const { directory } = fixture
  const productSessionId = 'psn_01J00000000000000000000035'
  const sensitiveValues = ['private-answer-first-7391', 'private-answer-second-2846']
  const script = `const input=ALL_TOOLS.find(tool=>tool.name.endsWith("request_user_input"));
    if(!input) throw new Error("input tool missing");
    await tools[input.name]({questions:[
      {id:"private_first",header:"First",question:"First private choice?",options:[{label:"continue",description:"Continue."},{label:"revise",description:"Revise."}]},
      {id:"private_second",header:"Second",question:"Second private choice?",options:[{label:"continue",description:"Continue."},{label:"revise",description:"Revise."}]}
    ]});
    { ${completedCommandScript("printf '%s' 'complete' > CHAT-INPUT-CONSUMED.txt")} }`
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...apiProductionScenarioBinaries(),
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: { 'native-cell-A': script } },
    scenario: { async run(context) {
      const { api, devicePath } = context
      await createAnchoredChat(context, productSessionId, 'Private multiquestion input')
      const requestId = 'req_01J00000000000000000000035'
      await api.command('chat.submit', 1, { productSessionId, message: 'native-cell-A' }, requestId)
      const inputs = () => api.query('session.interactions.list', { productSessionId, states: ['pending', 'resolved'] })
      const original = await waitFor(async () => {
        const interactions = (await inputs()).result.items.filter(item => item.kind === 'input')
        return interactions.length === 2 && interactions.every(item => item.state === 'pending') ? interactions : false
      }, 'both actual Core questions reach their public projections', 60_000)
      assert.equal(new Set(original.map(input => input.inputRequestId)).size, 2)
      assert.deepEqual(new Set(original.map(input => input.binding.executionJobId)), new Set([original[0].binding.executionJobId]))
      assert.ok(original.every(input => input.mode === 'single_choice' && input.allowOther === true))
      const ordered = ['First private choice?', 'Second private choice?'].map(prompt => original.find(input => input.prompt === prompt))
      assert.ok(ordered.every(Boolean), 'the original Core question identities are retained')
      let firstReceipt
      let firstPayload
      for (const [index, input] of ordered.entries()) {
        const payload = { ...input.binding, inputRequestId: input.inputRequestId,
          status: 'provided', value: { mode: 'single_choice', value: sensitiveValues[index] } }
        const answerRequestId = index === 0 ? 'req_01J00000000000000000000036' : 'req_01J00000000000000000000037'
        const accepted = await api.command('input.respond', input.revision, payload, answerRequestId)
        const replay = await api.command('input.respond', input.revision, payload, answerRequestId)
        assert.deepEqual(replay.result, accepted.result, 'an exact public response replay preserves its receipt')
        await assert.rejects(api.command('input.respond', input.revision, {
          ...payload, value: { mode: 'single_choice', value: 'changed-private-answer' },
        }, answerRequestId), error => error.status === 409)
        if (index === 0) {
          firstReceipt = accepted
          firstPayload = payload
          const partial = await waitFor(() => {
            const calls = retainedInputCalls(devicePath.deviceData)
            return calls.length === 1 && Object.keys(calls[0].answers).length === 1 ? calls[0] : false
          }, 'first accepted value is durably retained without completing the entire Core call', 30_000)
          assert.equal(partial.appliedIncarnationId, null, 'one answer cannot settle a two-question waiter')
          const current = (await inputs()).result.items
          assert.equal(current.filter(item => item.kind === 'input' && item.state === 'pending').length, 1)
          assert.equal(current.filter(item => item.kind === 'input' && item.state === 'provided').length, 1)
          assert.ok(sensitiveValues.every(value => !JSON.stringify(current).includes(value)), 'public interaction projections exclude accepted values')
        }
      }
      const completed = await waitFor(async () => {
        const calls = retainedInputCalls(devicePath.deviceData)
        const messages = (await api.query('session.messages.list', { productSessionId })).result.items
        const runtime = (await api.query('runtime.projection.get', { kind: 'product-session', productSessionId })).result.sessions
          .find(session => session.productSessionId === productSessionId)
        const interactions = (await inputs()).result.items
        return calls.length === 1 && Object.keys(calls[0].answers).length === 2 && calls[0].appliedIncarnationId
          && messages.some(message => message.role === 'assistant' && message.state === 'completed')
          && runtime?.activities.some(row => row.coreTool?.call?.toolName.endsWith('exec_command')
            && row.coreTool.call.execution === 'completed' && row.coreTool.call.disposition === 'accepted')
          && interactions.filter(item => item.kind === 'input' && item.state === 'provided').length === 2
          ? { call: calls[0], messages, runtime, interactions } : false
      }, 'complete retained values resume the original Core waiter and its next real command', 120_000)
      assert.equal(completed.call.questions.length, 2)
      assert.equal(completed.call.appliedIncarnationId, completed.call.readyIncarnationId)
      for (const [index, input] of ordered.entries()) {
        assert.ok(completed.call.answers[input.inputRequestId]?.values.length === 1
          && completed.call.answers[input.inputRequestId].values[0] === sensitiveValues[index],
        'the private Adapter preserves each original accepted value')
      }
      const publicProjections = JSON.stringify({ messages: completed.messages,
        runtime: completed.runtime, interactions: completed.interactions })
      assert.ok(sensitiveValues.every(value => !publicProjections.includes(value)), 'public Chat and runtime projections exclude sensitive answers')
      const requestsBeforeReplay = devicePath.modelServer.requests.length
      const terminalReplay = await api.command('input.respond', ordered[0].revision, firstPayload,
        'req_01J00000000000000000000036')
      assert.deepEqual(terminalReplay.result, firstReceipt.result)
      assert.equal(devicePath.modelServer.requests.length, requestsBeforeReplay, 'completed response replay does not resume another Core call')
      assert.equal(retainedInputCalls(devicePath.deviceData).length, 1)
      return { executionJobId: ordered[0].binding.executionJobId,
        questionCount: completed.call.questions.length, answerCount: Object.keys(completed.call.answers).length,
        applied: Boolean(completed.call.appliedIncarnationId), terminal: 'completed' }
    } },
  })
  assert.equal(report.flow.scenario.applied, true)
  assert.equal(report.flow.scenario.answerCount, 2)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  fixture.complete()
})

test('Chat expires its original real Core input deadline after heartbeat renewal and rejects a late answer', async t => {
  const fixture = createChatScenario(t, 'wwc-chat-input-deadline-')
  const { directory } = fixture
  const productSessionId = 'psn_01J00000000000000000000055'
  const script = `const input=ALL_TOOLS.find(tool=>tool.name.endsWith("request_user_input"));
    if(!input) throw new Error("input tool missing");
    text(await tools[input.name]({questions:[{id:"original_deadline",header:"Deadline",
      question:"Wait for this original deadline?",options:[{label:"continue",description:"Continue."},
      {label:"revise",description:"Revise."}]}]}));
    { ${completedCommandScript("printf '%s' 'consumed' > CHAT-INPUT-EXPIRED.txt")} }`
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...apiProductionScenarioBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: { 'native-cell-A': script } },
    scenario: { async run(context) {
      const { api, devicePath } = context
      await createAnchoredChat(context, productSessionId, 'Original input deadline')
      await api.command('chat.submit', 1, { productSessionId, message: 'native-cell-A' })
      const interactions = () => api.query('session.interactions.list', {
        productSessionId, states: ['pending', 'expired', 'resolved', 'cancelled'],
      })
      const original = await waitFor(async () => (await interactions()).result.items
        .find(item => item.kind === 'input' && item.state === 'pending'),
      'the real Core input reaches its original public deadline', 60_000)
      const jobId = original.binding.executionJobId
      const database = new DatabaseSync(join(directory, 'server-data', 'control-plane.sqlite3'), { readOnly: true })
      database.exec('PRAGMA busy_timeout=5000')
      try {
        const [retained] = retainedInteractionRequests(devicePath.deviceData, jobId)
        assert.equal(retained.inputRequestId, original.inputRequestId)
        assert.equal(retained.expiresAt, original.expiresAt)
        const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(jobId)
        const renewed = await waitFor(() => {
          const current = currentExecutionLease(database, authority, retained.lease)
          return current && Date.parse(current.expires_at) > Date.parse(original.expiresAt) ? current : false
        }, 'real heartbeats extend the execution lease while the original input stays pending', 30_000)
        assert.equal(authority.worker_session_id, original.binding.workerSessionId)
        const stillPending = (await interactions()).result.items.find(item => item.inputRequestId === original.inputRequestId)
        assert.equal(stillPending.expiresAt, original.expiresAt, 'lease renewal does not change the input deadline')
        const settled = await waitFor(async () => {
          const run = retainedCodexRuns(devicePath.deviceData, jobId)[0]
          const timeout = run?.interactionTimeouts?.find(item => item.request.inputRequestId === original.inputRequestId)
          const call = retainedInputCalls(devicePath.deviceData)
            .find(item => item.questions.some(question => question.inputRequestId === original.inputRequestId))
          const expired = (await interactions()).result.items.find(item => item.inputRequestId === original.inputRequestId)
          const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(jobId)
          const reservation = database.prepare('SELECT state FROM execution_admission_reservations WHERE job_id=?').get(jobId)
          const messages = (await api.query('session.messages.list', { productSessionId })).result.items
          return timeout?.appliedKernelSessionId && call?.appliedIncarnationId && expired?.state === 'expired'
            && terminal?.outcome === 'completed' && reservation?.state === 'settled'
            && messages.some(message => message.role === 'assistant' && message.state === 'completed')
            ? { run, timeout, call, expired, terminal } : false
        }, 'the original expired input settles its exact Core waiter and the original execution', 120_000)
        assert.equal(settled.timeout.causeCode, 'INTERACTION_DEADLINE_EXPIRED')
        assert.deepEqual(settled.timeout.request, retained, 'timeout retains the exact original request frame')
        assert.equal(settled.timeout.appliedKernelSessionId, settled.run.kernelSessionId)
        assert.equal(settled.call.appliedIncarnationId, settled.call.readyIncarnationId)
        assert.ok(Date.parse(settled.timeout.observedAt) >= Date.parse(original.expiresAt))
        assert.equal(settled.call.expiresAt, original.expiresAt)
        assert.deepEqual(settled.call.answers[original.inputRequestId].values, [])
        assert.equal(settled.run.interactionTimeouts.length, 1)
        assert.equal(settled.expired.expiresAt, original.expiresAt)
        assertSuccessfulCoreCommand(devicePath.deviceData, jobId, "printf '%s' 'consumed' > CHAT-INPUT-EXPIRED.txt")
        assertFrozenChatFile(devicePath.deviceData, jobId, 'CHAT-INPUT-EXPIRED.txt', 'consumed')
        await assert.rejects(api.command('input.respond', settled.expired.revision, {
          ...original.binding, inputRequestId: original.inputRequestId, status: 'provided',
          value: { mode: 'single_choice', value: 'continue' },
        }), error => Number.isInteger(error.status) && error.status >= 400 && error.status < 500)
        assert.equal(database.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id=?').get(jobId).count, 1)
        const expiredRun = retainedCodexRuns(devicePath.deviceData, jobId)[0]
        assert.equal(expiredRun.interactionTimeouts.length, 1)
        assert.equal(settled.timeout.appliedKernelSessionId, expiredRun.kernelSessionId)
        assert.deepEqual(retainedInputCalls(devicePath.deviceData)
          .find(item => item.questions.some(question => question.inputRequestId === original.inputRequestId))
          .answers[original.inputRequestId].values, [])
        return { jobId, inputRequestId: original.inputRequestId, expiresAt: original.expiresAt,
          renewedLeaseExpiresAt: renewed.expires_at, timeoutApplied: true, terminal: settled.terminal.outcome }
      } finally { database.close() }
    } },
  })
  assert.equal(report.flow.scenario.timeoutApplied, true)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  fixture.complete()
})

test('Chat resolves real Core approval only through its public decision and fences cancellation and the original expired deadline', async t => {
  const fixture = createChatScenario(t, 'wwc-chat-approval-deadline-')
  const { directory } = fixture
  const ids = { approved: 'psn_01J00000000000000000000056',
    cancelled: 'psn_01J00000000000000000000057', expired: 'psn_01J00000000000000000000058' }
  const observations = join(directory, 'approval-observations')
  mkdirSync(observations, { mode: 0o700 })
  assert.equal(statSync(observations).mode & 0o777, 0o700)
  const blockedMarker = join(observations, 'blocked-operation.txt')
  const continuationCommand = "printf '%s' 'continued' > CHAT-APPROVAL-CONTINUED.txt"
  const source = marker => completedCommandScript(`printf '%s' 'approved' > '${marker.replaceAll("'", "'\\''")}'`, 1,
    { sandbox_permissions: 'require_escalated', justification: 'Exercise the temporary Chat approval fixture.' })
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...apiProductionScenarioBinaries(),
    serverEnvironment: { WWC_SERVER_EXECUTION_LEASE_SECONDS: '30' },
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts: {
      'native-cell-A': source('CHAT-APPROVAL-EXECUTED.txt'),
      'native-cell-B': `
        try { ${source(blockedMarker)} } catch (error) {
          const reason = String(error?.message ?? error);
          const expired = "INTERACTION_DEADLINE_EXPIRED: approval expired; continue within the existing permissions";
          if (!reason.includes(expired)) throw error;
          text({ approvalExpired: expired });
        }
        { ${completedCommandScript(continuationCommand)} }`,
    } },
    scenario: { async run(context) {
      const { api, devicePath } = context
      const database = new DatabaseSync(join(directory, 'server-data', 'control-plane.sqlite3'), { readOnly: true })
      database.exec('PRAGMA busy_timeout=5000')
      const pending = async (productSessionId, marker) => {
        await createAnchoredChat(context, productSessionId, 'Real Core approval lifecycle')
        await api.command('chat.submit', 1, { productSessionId, message: marker })
        const approval = await waitFor(async () => (await api.query('approval.list', { states: ['pending'] })).result.items
          .find(item => item.binding.productSessionId === productSessionId),
        'the real escalated Core shell reaches its public approval projection', 60_000)
        assert.equal(approval.category, 'shell')
        assert.equal(approval.decisionEnabled, true, 'the actual trusted Core action supplies approval authority')
        return approval
      }
      const settle = async (approval, outcome) => waitFor(async () => {
        const jobId = approval.binding.executionJobId
        const terminal = database.prepare('SELECT * FROM execution_lease_terminals WHERE job_id=?').get(jobId)
        const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE job_id=?').get(jobId)
        const reservation = database.prepare('SELECT * FROM execution_admission_reservations WHERE job_id=?').get(jobId)
        const messages = (await api.query('session.messages.list', { productSessionId: approval.binding.productSessionId })).result.items
        const queueState = outcome === 'cancelled' ? 'failed' : outcome
        return terminal?.outcome === outcome && job?.state === queueState
          && reservation?.state === (outcome === 'cancelled' ? 'released' : 'settled')
          && messages.some(message => message.role === 'assistant' && message.state === outcome)
          ? { job, terminal, reservation } : false
      }, `${outcome} approval execution closes its original durable terminal and reservation`, 120_000)
      const rejectLateDecision = approval => assert.rejects(api.command('approval.decide', approval.revision, {
        approvalId: approval.id, binding: approval.binding, decision: 'approve',
        reason: 'This late fixture decision must be rejected.',
      }), error => Number.isInteger(error.status) && error.status >= 400 && error.status < 500)
      try {
        const approved = await pending(ids.approved, 'native-cell-A')
        assert.equal(retainedCodexRuns(devicePath.deviceData, approved.binding.executionJobId)[0].terminal, null)
        assert.equal(globSync('**/CHAT-APPROVAL-EXECUTED.txt', { cwd: directory }).length, 0,
          'the formally pending approval has not executed its original shell operation')
        const decisionPayload = { approvalId: approved.id, binding: approved.binding, decision: 'approve',
          reason: 'Run the temporary approval acceptance command.' }
        const decisionRequestId = 'req_01J00000000000000000000056'
        const accepted = await api.command('approval.decide', approved.revision, decisionPayload, decisionRequestId)
        assert.equal(accepted.outcome, 'completed')
        const approvedTerminal = await settle(approved, 'completed')
        assertSuccessfulCoreCommand(devicePath.deviceData, approved.binding.executionJobId,
          "printf '%s' 'approved' > 'CHAT-APPROVAL-EXECUTED.txt'")
        assertFrozenChatFile(devicePath.deviceData, approved.binding.executionJobId, 'CHAT-APPROVAL-EXECUTED.txt', 'approved')
        const httpBeforeReplay = devicePath.modelServer.requests.length
        const replay = await api.command('approval.decide', approved.revision, decisionPayload, decisionRequestId)
        assert.deepEqual(replay.result, accepted.result)
        assert.equal(devicePath.modelServer.requests.length, httpBeforeReplay)
        assert.equal((await api.query('approval.get', { approvalId: approved.id })).result.state, 'approved')

        const cancelled = await pending(ids.cancelled, 'native-cell-B')
        assert.equal(existsSync(blockedMarker), false)
        const current = (await api.query('session.get', { productSessionId: ids.cancelled })).result
        await api.command('session.cancel', current.revision, {
          productSessionId: ids.cancelled, reason: 'Cancel the original observed approval waiter',
        })
        const cancelledTerminal = await settle(cancelled, 'cancelled')
        assert.equal(cancelledTerminal.reservation.release_reason, 'cancelled')
        await rejectLateDecision(cancelled)
        assert.equal(existsSync(blockedMarker), false, 'cancellation does not execute the denied shell')
        const cancelledCommands = coreCommandAttempts(devicePath.deviceData, cancelled.binding.executionJobId)
        assert.equal(cancelledCommands.filter(row => row.command === continuationCommand).length, 0,
          'the cancelled original Core cell never starts its continuation command')

        const expired = await pending(ids.expired, 'native-cell-B')
        assert.equal(existsSync(blockedMarker), false)
        const jobId = expired.binding.executionJobId
        const [retained] = retainedInteractionRequests(devicePath.deviceData, jobId)
        assert.equal(retained.approvalId, expired.id)
        assert.equal(retained.expiresAt, expired.expiresAt)
        const authority = database.prepare('SELECT * FROM execution_dispatch_authorities WHERE job_id=?').get(jobId)
        const renewed = await waitFor(() => {
          const current = currentExecutionLease(database, authority, retained.lease)
          return current && Date.parse(current.expires_at) > Date.parse(expired.expiresAt) ? current : false
        }, 'real heartbeats renew the lease while its original Core approval remains pending', 30_000)
        assert.equal((await api.query('approval.get', { approvalId: expired.id })).result.expiresAt, expired.expiresAt)
        const timeout = await waitFor(() => retainedCodexRuns(devicePath.deviceData, jobId)[0]?.interactionTimeouts
          ?.find(item => item.request.approvalId === expired.id && item.appliedKernelSessionId),
        'the original approval deadline submits denial to its exact Core waiter', 120_000)
        assert.equal(timeout.causeCode, 'INTERACTION_DEADLINE_EXPIRED')
        assert.deepEqual(timeout.request, retained, 'expiry retains the original approval authority frame')
        assert.ok(Date.parse(timeout.observedAt) >= Date.parse(expired.expiresAt))
        const expiredTerminal = await settle(expired, 'completed')
        const expiredProjection = (await api.query('approval.get', { approvalId: expired.id })).result
        assert.equal(expiredProjection.state, 'expired')
        assert.equal(expiredProjection.expiresAt, expired.expiresAt)
        await rejectLateDecision(expiredProjection)
        assert.equal(existsSync(blockedMarker), false, 'expiry denies the original real escalated command')
        assertSuccessfulCoreCommand(devicePath.deviceData, jobId, continuationCommand)
        assertFrozenChatFile(devicePath.deviceData, jobId, 'CHAT-APPROVAL-CONTINUED.txt', 'continued')
        const expiredRun = retainedCodexRuns(devicePath.deviceData, jobId)[0]
        assert.equal(expiredRun.interactionTimeouts.length, 1)
        assert.equal(timeout.appliedKernelSessionId, expiredRun.kernelSessionId)
        for (const terminal of [approvedTerminal, cancelledTerminal, expiredTerminal]) {
          assert.equal(terminal.job.attempt, 1)
          assert.equal(database.prepare('SELECT COUNT(*) AS count FROM execution_lease_terminals WHERE job_id=?')
            .get(terminal.job.job_id).count, 1)
        }
        assert.equal(existsSync(blockedMarker), false)
        assert.deepEqual(coreCommandAttempts(devicePath.deviceData, cancelled.binding.executionJobId), cancelledCommands)
        return { approvedJobId: approvedTerminal.job.job_id, cancelledJobId: cancelledTerminal.job.job_id,
          expiredJobId: jobId, approvalExpiresAt: expired.expiresAt, renewedLeaseExpiresAt: renewed.expires_at,
          timeoutApplied: true, terminal: expiredTerminal.terminal.outcome }
      } finally { database.close() }
    } },
  })
  assert.equal(report.flow.scenario.timeoutApplied, true)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  fixture.complete()
})

function retainedDeviceModelRequests(deviceData, jobId) {
  return globSync('**/worker-codex.sqlite3', { cwd: deviceData }).flatMap(path => {
    const store = new DatabaseSync(join(deviceData, path), { readOnly: true })
    try {
      store.exec('PRAGMA busy_timeout=5000')
      if (!store.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='execution_outbox'").get()) return []
      return store.prepare("SELECT frame_json, state FROM execution_outbox WHERE family='model_open'").all()
        .map(row => ({ frame: JSON.parse(Buffer.from(row.frame_json).toString('utf8')), state: row.state }))
        .filter(row => row.frame.lease.jobId === jobId)
        .map(row => ({ modelExchangeId: row.frame.modelExchangeId, state: row.state }))
    } finally { store.close() }
  })
}

test('Chat keeps three real Provider HTTP attempts in flight while durable retry and a cancelled fourth waiter respect slots', async t => {
  const fixture = createChatScenario(t, 'wwc-chat-provider-slots-')
  const { directory } = fixture
  const heldMarkers = ['concurrency-first', 'concurrency-second', 'concurrency-third']
  const retryMarker = 'concurrency-retry'
  const cancelMarker = 'concurrency-cancel'
  const sessionIds = [
    'psn_01J00000000000000000000041', 'psn_01J00000000000000000000042',
    'psn_01J00000000000000000000043', 'psn_01J00000000000000000000044',
    'psn_01J00000000000000000000045', 'psn_01J00000000000000000000046',
  ]
  const requestIds = [
    'req_01J00000000000000000000041', 'req_01J00000000000000000000042',
    'req_01J00000000000000000000043', 'req_01J00000000000000000000044',
    'req_01J00000000000000000000045', 'req_01J00000000000000000000046',
  ]
  const gateMarkers = [...heldMarkers, retryMarker, cancelMarker]
  const report = await runApiProductionVertical({
    directory, restart: false, repeat: false, ...apiProductionScenarioBinaries(),
    deviceProvider: { ...deterministicDeviceProvider(), responseGateMarkers: gateMarkers,
      httpFailures: { [retryMarker]: { status: 503, count: 1, retryAfterSeconds: 30 } } },
    scenario: { async run(context) {
      const { api, devicePath } = context
      const database = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'), { readOnly: true })
      const providers = []
      const server = devicePath.modelServer
      const submitted = []
      const launch = async (index, marker) => {
        await createAnchoredChat(context, sessionIds[index], marker)
        const payload = { productSessionId: sessionIds[index], message: marker }
        const receipt = await api.command('chat.submit', 1, payload, requestIds[index])
        const job = database.prepare('SELECT * FROM scheduler_execution_jobs WHERE submission_request_id = ?').get(requestIds[index])
        assert.ok(job)
        submitted[index] = { receipt, payload, job }
        return job
      }
      const settled = async index => {
        const job = submitted[index].job
        const current = database.prepare('SELECT state FROM scheduler_execution_jobs WHERE job_id = ?').get(job.job_id)
        const terminal = database.prepare('SELECT outcome FROM execution_lease_terminals WHERE job_id = ?').get(job.job_id)
        const reservation = database.prepare('SELECT state FROM execution_admission_reservations WHERE job_id = ?').get(job.job_id)
        const expectedReservation = index === 3 ? 'released' : 'settled'
        const messages = (await api.query('session.messages.list', { productSessionId: sessionIds[index] })).result.items
        return current?.state === 'completed' && terminal?.outcome === 'completed' && reservation?.state === expectedReservation
          && messages.some(message => message.role === 'assistant' && message.state === 'completed')
      }
      try {
        for (const index of [0, 1]) await launch(index, heldMarkers[index])
        await waitFor(() => heldMarkers.slice(0, 2).every(marker => server.requests
          .some(request => request.gateMarker === marker && request.active)), 'two real HTTP requests retain their Provider slots', 60_000)
        providers.push(...deviceProviderDatabases(devicePath.deviceData, { readOnly: true }))
        const retryJob = await launch(3, retryMarker)
        const retry = await waitFor(() => providers.flatMap(store => store.prepare(`
          SELECT r.exchange_id, r.wake_ms, r.policy_attempt, e.request_open
          FROM model_retry_state r JOIN exchanges e ON e.exchange_id=r.exchange_id`).all())
          .find(row => JSON.parse(row.request_open).lease.jobId === retryJob.job_id
            && row.wake_ms > Date.now() + 5000), 'real 503 leaves one durable retry waiting while its physical slot is released', 60_000)
        assert.deepEqual(server.requests.filter(request => request.gateMarker === retryMarker).map(request => request.status), [503])
        await launch(2, heldMarkers[2])
        await waitFor(() => heldMarkers.every(marker => server.requests
          .some(request => request.gateMarker === marker && request.active)), 'three physical HTTPS attempts are simultaneously in flight', 60_000)
        assert.equal(server.activeRequests, 3)
        assert.equal(server.peakActiveRequests, 3)
        const fourth = await launch(4, cancelMarker)
        const waiting = await waitFor(() => {
          const retained = retainedDeviceModelRequests(devicePath.deviceData, fourth.job_id)
          return retained.length === 1 ? retained[0] : false
        }, 'fourth real Core ModelOpen reaches its durable Worker queue without a physical slot', 60_000)
        await waitFor(() => Date.now() >= retry.wake_ms + 500,
          'durable retry becomes due while all three physical slots remain occupied', 45_000)
        assert.equal(server.requests.filter(request => request.gateMarker === retryMarker).length, 1,
          'a due durable retry cannot make a fourth HTTP request')
        assert.equal(server.requests.filter(request => request.gateMarker === cancelMarker).length, 0,
          'a newly queued fourth request cannot bypass the same physical slots')
        assert.equal(server.activeRequests, 3)
        const session = (await api.query('session.get', { productSessionId: sessionIds[4] })).result
        await api.command('session.cancel', session.revision, {
          productSessionId: sessionIds[4], reason: 'Cancel the observed Provider slot waiter',
        }, 'req_01J00000000000000000000047')
        await waitFor(async () => {
          const terminal = database.prepare('SELECT outcome FROM execution_lease_terminals WHERE job_id = ?').get(fourth.job_id)
          const reservation = database.prepare('SELECT state,release_reason FROM execution_admission_reservations WHERE job_id = ?').get(fourth.job_id)
          const messages = (await api.query('session.messages.list', { productSessionId: sessionIds[4] })).result.items
          return terminal?.outcome === 'cancelled' && reservation?.state === 'released'
            && reservation?.release_reason === 'cancelled'
            && messages.some(message => message.role === 'assistant' && message.state === 'cancelled')
        }, 'slot waiting cancellation settles through the original Chat execution', 60_000)
        server.releaseResponseGate(heldMarkers[0])
        await waitFor(() => server.requests.some(request => request.gateMarker === retryMarker
          && request.status === 200 && request.active), 'durable retry takes the released third slot for its next real HTTP attempt', 60_000)
        assert.ok(server.activeRequests <= 3)
        assert.equal(server.peakActiveRequests, 3)
        assert.equal(server.requests.filter(request => request.gateMarker === cancelMarker).length, 0,
          'the cancelled waiter stays fenced after a physical slot is released')
        for (const marker of gateMarkers) server.releaseResponseGate(marker)
        await waitFor(async () => {
          for (const index of [0, 1, 2, 3]) if (!await settled(index)) return false
          return true
        }, 'all admitted original Chat executions complete and release their physical slots', 120_000)
        const calls = readDeviceExecutionReceipts(directory).calls
        const retryCalls = calls.filter(call => call.jobId === retryJob.job_id)
        assert.equal(retryCalls.length, 1)
        assert.deepEqual(retryCalls[0].providerAttempts.map(attempt => attempt.state), ['failed', 'completed'])
        assert.equal(retryCalls[0].providerAttempts[0].usage, null)
        await assertUnknownExecutionAccounting(database, retryJob.job_id,
          retryCalls[0].providerAttempts[1].usage.totalTokens)
        assert.equal(retryJob.attempt, 1, 'Provider recovery preserves the original execution attempt')
        for (const index of [0, 1, 2]) {
          const originalCalls = calls.filter(call => call.jobId === submitted[index].job.job_id)
          assert.equal(originalCalls.length, 1)
          assert.deepEqual(originalCalls[0].providerAttempts.map(attempt => attempt.state), ['completed'])
        }
        assert.equal(calls.filter(call => call.jobId === fourth.job_id)
          .flatMap(call => call.providerAttempts).filter(attempt => !['prepared', 'not_sent'].includes(attempt.state)).length, 0)
        await launch(5, 'concurrency-probe')
        await waitFor(() => settled(5), 'a fresh Chat uses the released Provider slots', 60_000)
        const httpCount = server.requests.length
        const replayed = await api.command('chat.submit', 1, submitted[3].payload, requestIds[3])
        assert.deepEqual(replayed.result, submitted[3].receipt.result)
        assert.equal(server.requests.length, httpCount, 'completed replay acquires no slot and sends no HTTP')
        assert.equal(server.activeRequests, 0)
        assert.equal(server.peakActiveRequests, 3)
        return { peakHttp: server.peakActiveRequests, httpRequests: httpCount,
          retryJobId: retryJob.job_id, retryPhysicalAttempts: retryCalls[0].providerAttempts.length,
          cancelledJobId: fourth.job_id, cancelledModelExchangeId: waiting.modelExchangeId,
          cancelledHttpRequests: 0, probeCompleted: true }
      } finally {
        for (const marker of gateMarkers) server.releaseResponseGate(marker)
        for (const store of providers) store.close()
        database.close()
      }
    } },
  })
  assert.equal(report.flow.scenario.peakHttp, 3)
  assert.equal(report.flow.scenario.cancelledHttpRequests, 0)
  t.diagnostic(JSON.stringify(report.flow.scenario))
  fixture.complete()
})

test('public API cancellation closes an active native cell and preserves an independent Session', async t => {
  const installed = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR
  const directory = mkdtempSync(join(tmpdir(), 'wwc-native-cancel-'))
  t.after(() => rmSync(directory, { recursive: true, force: true }))
  const ids = { A: 'psn_01J00000000000000000000011', B: 'psn_01J00000000000000000000012' }
  const nativeCellScripts = Object.fromEntries(Object.entries(ids).map(([tag, id]) => [
    `native-cell-${tag}`, `// @exec: {"yield_time_ms":1000}\nconst tool=ALL_TOOLS.find(t=>t.name.endsWith(${JSON.stringify(`benchmark_public_smoke_${id}__public_smoke`)})); if(!tool) throw new Error('fixture missing'); text(await tools[tool.name]({tag:${JSON.stringify(tag)},phase:'hold'})); text(await tools[tool.name]({tag:${JSON.stringify(tag)},phase:'after'}));`,
  ]))
  const calls = () => existsSync(join(directory, 'calls.jsonl'))
    ? readFileSync(join(directory, 'calls.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(line => JSON.parse(line)) : []
  const report = await runApiProductionVertical({
    directory, build: installed === undefined && process.env.WWC_API_SKIP_BUILD !== '1', restart: false, repeat: false,
    ...(installed === undefined ? {} : {
      serverBinary: join(installed, 'winwincode-server'),
      wwcBinary: join(installed, 'wwc'),
    }),
    deviceProvider: { ...deterministicDeviceProvider(), nativeCellScripts },
    scenario: { async run({ api, modelRoute, devicePath }) {
      const activities = async id => (await api.query('runtime.projection.get', {
        kind: 'product-session', productSessionId: id,
      })).result.sessions.find(session => session.productSessionId === id)?.activities ?? []
      const approve = async id => {
        const pending = (await api.query('approval.list', { states: ['pending'] })).result.items
        for (const row of pending) {
          const approval = (await api.query('approval.get', { approvalId: row.id })).result
          if (approval.binding?.productSessionId !== id || !approval.decisionEnabled) continue
          assert.equal(approval.category, 'mcp')
          assert.deepEqual(approval.sanitizedDetail.targetSummaries, [`server:benchmark_public_smoke_${id}`])
          const decided = await api.command('approval.decide', approval.revision, {
            approvalId: approval.id, binding: approval.binding, decision: 'approve',
            reason: 'Run this Session cancellation fixture.',
          })
          assert.equal(decided.outcome, 'completed')
        }
      }
      const live = {}
      for (const [tag, id] of Object.entries(ids)) {
        await installDevicePublicSmoke({ api, publicClientId: devicePath.publicClientId,
          id: `benchmark_public_smoke_${id}`, configuration: {
            command: process.execPath, args: [resolve('tests/fixtures/code-mode-cancellation-mcp.mjs'), directory],
          } })
        const created = await api.command('session.create', 0, { productSessionId: id,
          projectId: 'prj_01J00000000000000000000000', repositoryId: 'rep_01J00000000000000000000000',
          title: `Native cell ${tag}`, modelRoute })
        assert.equal(created.outcome, 'completed')
        await devicePath.launchAnchor({ productSessionId: id })
        const submitted = await api.command('chat.submit', 1, { productSessionId: id, message: `native-cell-${tag}` })
        assert.equal(submitted.outcome, 'completed')
        live[tag] = await waitFor(async () => {
          await approve(id)
          const rows = await activities(id)
          const cellRow = rows.find(row => row.coreTool?.cell?.lifecycle === 'live')
          const cell = cellRow?.coreTool.cell
          const nested = rows.find(row => row.coreTool?.call?.toolName.endsWith('public_smoke')
            && row.coreTool.call.execution === 'running')?.coreTool.call
          return cell && nested && calls().some(call => call.tag === tag && call.phase === 'hold' && call.state === 'started')
            ? { cell, nested, sourceThreadId: cellRow.coreTool.sourceThreadId } : false
        }, `native cell ${tag} entered a real blocked MCP call`, 60_000)
        assert.equal(live[tag].nested.cellId, live[tag].cell.cellId)
        assert.ok(live[tag].nested.parentRequestSequence > 0)
      }
      assert.notEqual(live.A.sourceThreadId, live.B.sourceThreadId)
      const current = (await api.query('session.get', { productSessionId: ids.A })).result
      const cancelled = await api.command('session.cancel', current.revision, {
        productSessionId: ids.A, reason: 'Cancel the observed active native cell',
      })
      assert.equal(cancelled.result.state, 'cancelled')
      try {
        await waitFor(async () => (await activities(ids.A)).some(row => row.coreTool?.cell?.cellId === live.A.cell.cellId
          && row.coreTool.cell.lifecycle === 'closed'), 'cancelled cell closed in the Core projection', 60_000)
        assert.ok(!(await activities(ids.B)).some(row => row.coreTool?.cell?.cellId === live.B.cell.cellId
          && row.coreTool.cell.lifecycle === 'closed'), 'Session B remains active while A closes')
        writeFileSync(join(directory, 'A.release'), '')
        writeFileSync(join(directory, 'B.release'), '')
        await waitFor(async () => {
          await approve(ids.B)
          const messages = (await api.query('session.messages.list', { productSessionId: ids.B })).result.items
          return messages.some(message => message.role === 'assistant' && message.state === 'completed'
            && message.content.includes('native-cell-B completed'))
        }, 'independent Session B completed after A cancellation', 60_000)
      } catch (error) {
        const sessions = await Promise.all(Object.entries(ids).map(async ([tag, id]) => {
          const session = (await api.query('session.get', { productSessionId: id })).result
          const rows = (await activities(id)).map(row => row.coreTool).filter(Boolean)
          const messages = (await api.query('session.messages.list', { productSessionId: id })).result.items
          return { tag, state: session.state, revision: session.revision,
            messages: messages.slice(-8).map(message => ({ role: message.role, state: message.state,
              completionMarkerObserved: message.role === 'assistant' && message.content.includes(`native-cell-${tag} completed`),
            })),
            cells: rows.filter(row => row.cell).slice(-16).map(row => ({
              sourceThreadId: row.sourceThreadId, sourceSequence: row.sourceSequence, cell: row.cell,
            })),
            calls: rows.filter(row => row.call).slice(-16).map(row => ({
              sourceThreadId: row.sourceThreadId, sourceSequence: row.sourceSequence,
              requestSequence: row.call.requestSequence, toolName: row.call.toolName,
              execution: row.call.execution, disposition: row.call.disposition,
              delivery: row.call.delivery, cancelled: row.call.cancelled,
            })),
          }
        }))
        const evidence = { sessions, workers: cancellationEvidence(devicePath.deviceData),
          calls: calls().slice(-32).map(({ tag, phase, state }) => ({ tag, phase, state })) }
        throw new Error(`${error.message}\nCancellation evidence: ${JSON.stringify(evidence)}`, { cause: error })
      }
      assert.equal(calls().filter(call => call.tag === 'A' && call.phase === 'after').length, 0)
      assert.equal(calls().filter(call => call.tag === 'B' && call.phase === 'after' && call.state === 'completed').length, 1)
      const settledA = (await api.query('session.get', { productSessionId: ids.A })).result
      assert.equal(settledA.state, 'cancelled')
      return { cancelled: ids.A, completed: ids.B, live, calls: calls() }
    } },
  })
  assert.equal(report.flow.scenario.cancelled, ids.A)
  assert.equal(report.flow.scenario.completed, ids.B)
})


test('the actual task entry completes parallel Sessions in one Server and Device runtime', async t => {
  const directory = process.env.WWC_PARALLEL_FIXTURE_DIRECTORY ?? mkdtempSync(join(tmpdir(), 'wwc-parallel-tasks-'))
  mkdirSync(directory, { recursive: true })
  if (process.env.WWC_PARALLEL_FIXTURE_DIRECTORY === undefined) t.after(() => rmSync(directory, { recursive: true, force: true }))
  const oldDeterministic = process.env.WWC_DEVICE_TASK_DETERMINISTIC
  process.env.WWC_DEVICE_TASK_DETERMINISTIC = '1'
  t.after(() => {
    if (oldDeterministic === undefined) delete process.env.WWC_DEVICE_TASK_DETERMINISTIC
    else process.env.WWC_DEVICE_TASK_DETERMINISTIC = oldDeterministic
  })
  const task = { title: 'Independent task', goal: 'Write status: complete to TASK.md.',
    scope: ['TASK.md'], constraints: ['Only change TASK.md'], outOfScope: [],
    verificationCommand: 'npm run verify',
    acceptanceCriteria: [{ id: 'done', required: true, title: 'TASK.md is complete' }],
    files: { 'TASK.md': 'status: pending\n', 'package.json': JSON.stringify({
      private: true, scripts: { verify: "node -e \"if(require('fs').readFileSync('TASK.md','utf8')!=='status: complete\\n')process.exit(1)\"" },
    }) + '\n' } }
  const inputPath = join(directory, 'native-task.json')
  writeFileSync(inputPath, JSON.stringify(task))
  const profile = benchmarkConfiguration('main-A')
  const options = { directory, profiles: [profile],
    providers: [deterministicDeviceProvider()], build: process.env.WWC_API_SKIP_BUILD !== '1',
    timeoutMillis: 1_200_000,
  }
  let retained
  const result = await withDeviceTaskRuntime(options, async runtime => {
      const contexts = new Map()
      for (const id of ['first', 'second']) contexts.set(id, await runtime.forTask({
        ...profile, provider: 'device-deterministic-model',
      }, { taskInputPath: inputPath, directory: join(directory, `${id}-repository`) }))
      const context = contexts.get('first'), api = context.api
      const launch = async id => {
        const prior = join(directory, id, 'device-task-result.json')
        if (existsSync(prior)) {
          const retained = JSON.parse(readFileSync(prior, 'utf8'))
          assert.equal(retained.complete, true, 'an interrupted fixture task must not be replayed')
          const projection = (await api.query('delivery.get', { deliveryId: retained.deliveryId })).result
          assert.equal(projection.status, 'done')
          assert.equal(projection.verdict.status, 'pass')
          return retained
        }
        return runDeviceTaskVertical({ directory: join(directory, id),
          ...deviceTaskIdentities('native-parallel-entry', id), runtime: contexts.get(id),
          timeoutMillis: 1_200_000 })
      }
      const results = await Promise.allSettled([launch('first'), launch('second')])
      for (const result of results) if (result.status === 'rejected') throw result.reason
      const [first, second] = results.map(result => result.value)
      assert.equal(first.complete, true)
      assert.equal(second.complete, true)
      assert.equal(first.delivery.detail.verdict.status, 'pass')
      assert.equal(second.delivery.detail.verdict.status, 'pass')
      assert.notEqual(first.productSessionId, second.productSessionId)
      assert.notEqual(first.deliveryId, second.deliveryId)
      assert.equal(first.publicClientId, second.publicClientId)
      const receipts = value => readDeviceExecutionReceipts(value.directory, {
        deliveryId: value.deliveryId, productSessionId: value.productSessionId,
      })
      const a = receipts(first), b = receipts(second)
      assert.ok(a.calls.length > 0 && b.calls.length > 0)
      assert.equal(a.calls.some(call => b.calls.some(other => call.exchangeId === other.exchangeId)), false)
      assert.equal(a.performance.some(row => b.performance.some(other => row.runKey === other.runKey)), false)
      for (const task of [first, second]) {
        assert.equal(task.sourceAnchor.state, 'drained')
        const core = new DatabaseSync(join(contexts.get('first').devicePath.deviceData,
          'worker-sessions', task.sourceAnchor.workerSessionId, 'data/codex-runtime/worker-codex.sqlite3'),
        { readOnly: true })
        try { assert.equal(core.prepare('SELECT count(*) AS n FROM model_call_ledger').get().n, 0) }
        finally { core.close() }
        assert.equal(task.delivery.workRunAggregate.runs.length, 3)
        assert.equal(task.delivery.workRunAggregate.runs.every(run => run.state === 'settled'), true)
      }
      const registry = new DatabaseSync(join(directory, 'server-data/control-plane.sqlite3'))
      registry.exec('PRAGMA busy_timeout = 5000')
      try {
        const template = registry.prepare('SELECT * FROM execution_workers WHERE worker_id = ?').get(first.sourceAnchor.workerId)
        const scope = registry.prepare('SELECT * FROM execution_worker_scopes WHERE worker_id = ?').get(first.sourceAnchor.workerId)
        const columns = Object.keys(template)
        const insert = registry.prepare(`INSERT INTO execution_workers (${columns.join(',')}) VALUES (${columns.map(() => '?').join(',')})`)
        for (let i = 1; i <= 201; i++) {
          const workerId = `wrk_${String(i).padStart(26, '0')}`
          const instance = `wki_${String(i).padStart(26, '0')}`
          if (registry.prepare('SELECT 1 FROM execution_workers WHERE worker_id = ?').get(workerId)) continue
          insert.run(...columns.map(key => key === 'worker_id' ? workerId : key === 'worker_instance_id' ? instance : template[key]))
          registry.prepare('INSERT INTO execution_worker_instances VALUES (?,?,?)').run(workerId, instance, template.started_at)
          registry.prepare('INSERT INTO execution_worker_scopes VALUES (?,?,?)').run(workerId, scope.scope_key, scope.scope_json)
          assert.equal((await api.command('worker.drain', 0, { workerId, reason: 'Historical drained fixture' })).outcome, 'completed')
        }
      } finally { registry.close() }
      const firstPage = await api.query('worker.list', { states: [] })
      assert.equal(firstPage.result.items.length, 200)
      assert.equal(firstPage.page.hasMore, true)
      assert.equal(firstPage.result.items.every(worker => worker.state === 'draining'), true)
      retained = { first, second, receipts: [a, b],
        baseline: context.baseline, binding: context.repositoryBindingId,
        client: context.devicePath.publicClientId,
        endpoint: readFileSync(join(directory, 'server-endpoint.json'), 'utf8'),
        certificate: readFileSync(join(directory, 'fixture-cert.pem'), 'utf8') }
      return { completed: 2, calls: [a.calls.length, b.calls.length] }
  })
  assert.equal(result.flow.scenario.completed, 2)
  const reopened = await withDeviceTaskRuntime({ ...options, build: false }, async runtime => {
    const context = await runtime.forTask({ ...profile, provider: 'device-deterministic-model' },
      { taskInputPath: inputPath, directory: join(directory, 'first-repository') })
    assert.equal(context.baseline, retained.baseline)
    assert.equal(context.repositoryBindingId, retained.binding)
    assert.equal(context.devicePath.publicClientId, retained.client)
    assert.equal(readFileSync(join(directory, 'server-endpoint.json'), 'utf8'), retained.endpoint)
    assert.equal(readFileSync(join(directory, 'fixture-cert.pem'), 'utf8'), retained.certificate)
    for (const [index, prior] of [retained.first, retained.second].entries()) {
      const projection = (await context.api.query('delivery.get', { deliveryId: prior.deliveryId })).result
      assert.equal(projection.status, 'done')
      assert.equal(projection.verdict.status, 'pass')
      assert.deepEqual(readDeviceExecutionReceipts(prior.directory, {
        deliveryId: prior.deliveryId, productSessionId: prior.productSessionId,
      }).calls, retained.receipts[index].calls)
    }
    assert.equal(context.devicePath.modelServer.requests.length, 0, 'reopening completed calls must not invoke a model')
    const next = await runDeviceTaskVertical({ directory: join(directory, 'third'),
      ...deviceTaskIdentities('native-parallel-entry', 'third'), runtime: context,
      timeoutMillis: 1_200_000 })
    assert.equal(next.complete, true)
    assert.equal(next.delivery.detail.verdict.status, 'pass')
    const foreign = await context.api.query('worker.list', { states: [] })
    assert.equal(foreign.result.items.every(worker => worker.state === 'draining' && worker.revision === 1), true)
    return { retainedCompleted: 2, continued: 1 }
  })
  assert.deepEqual(reopened.flow.scenario, { retainedCompleted: 2, continued: 1 })
})
