// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { DatabaseSync } from 'node:sqlite'
import { classifyError, decide, executeFetch, executeRequest, httpFailure, NetworkError, withResponseFailure } from '../packages/network-request/src/index.mjs'
import { retainedRequestFailure } from '../scripts/device-model-failures.mjs'
import { runBenchmarkPlan } from '../scripts/run-real-task-benchmark.mjs'
import { ControlPlaneClientError, createControlPlaneHttpClient } from '../apps/client/src/generated/control-plane-client.ts'

const sensitive = 'SYNTHETIC_PRIVATE_PAYLOAD https://invalid.example/private?token=SYNTHETIC_CREDENTIAL'
const query = {
  schemaVersion: 'winwincode/v1', requestId: 'req_00000000000000000000000001',
  actor: { id: 'usr_00000000000000000000000001', kind: 'user' },
  scope: { kind: 'repository', organizationId: 'org_00000000000000000000000001',
    workspaceId: 'wsp_00000000000000000000000001', projectId: 'prj_00000000000000000000000001',
    repositoryId: 'rep_00000000000000000000000001' },
  query: 'delivery.list', parameters: { states: [] }, page: { cursor: null, limit: 25 },
}

function unavailableResponse(body = sensitive) {
  return { ok: false, status: 503, headers: { get: () => null }, text: async () => body }
}

function safeRequestFacts(request, count, stopReason = 'retry_budget_exhausted') {
  assert.ok(request, 'final failure needs safe request evidence')
  assert.equal(request.network.httpStatus, 503)
  assert.equal(request.attempts.length, count)
  assert.deepEqual(request.attempts.map(row => row.networkAttempt), Array.from({ length: count }, (_, index) => index + 1))
  assert.ok(request.attempts.every(row => row.failure.httpStatus === 503))
  assert.equal(request.stopReason, stopReason)
  assert.doesNotMatch(JSON.stringify(request), /SYNTHETIC_PRIVATE|SYNTHETIC_CREDENTIAL|invalid\.example/u)
}

test('HTTP503 response keeps non-enumerable retry facts through business error and durable runner failure', async t => {
  const directory = await mkdtemp(join(tmpdir(), 'wwc-http-runner-diagnostic-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  let sends = 0, response
  const business = Object.assign(new Error(sensitive), { code: 'PROVIDER_UNAVAILABLE' })
  const ledgerPath = join(directory, 'benchmark.sqlite3')
  const options = {
    ledgerPath, experimentBinding: { experimentId: 'http-diagnostic-fixture' },
    executeCell: async () => {
      response = await executeFetch(async () => { sends += 1; return unavailableResponse() },
        'https://invalid.example/?token=SYNTHETIC_CREDENTIAL', {}, {
          replay: 'retry_inference', maxAttempts: 3, waitBeforeRetry: async () => {},
        })
      assert.ok(response.networkError instanceof NetworkError)
      assert.equal(Object.getOwnPropertyDescriptor(response, 'networkError').enumerable, false)
      assert.equal(JSON.stringify(response).includes('networkError'), false)
      assert.equal(withResponseFailure(business, response), business)
      assert.equal(Object.getOwnPropertyDescriptor(business, 'cause').enumerable, false)
      throw business
    },
  }
  const plan = { cells: [{ runId: 'http-diagnostic' }] }
  const result = await runBenchmarkPlan(plan, options)
  assert.equal(sends, 3)
  assert.equal(result.records[0].status, 'failed')
  assert.equal(result.records[0].failure.code, 'PROVIDER_UNAVAILABLE')
  safeRequestFacts(result.records[0].failure.diagnostic.request, 3)
  assert.doesNotMatch(JSON.stringify(result), /SYNTHETIC_PRIVATE|SYNTHETIC_CREDENTIAL|invalid\.example/u)
  const db = new DatabaseSync(ledgerPath, { readOnly: true })
  t.after(() => db.close())
  const persisted = JSON.parse(db.prepare('SELECT record FROM benchmark_cell').get().record)
  assert.deepEqual(persisted.failure, result.records[0].failure)
  const replay = await runBenchmarkPlan(plan, { ...options, executeCell: () => assert.fail('durable failure must not resend') })
  assert.deepEqual(replay, result)
  assert.equal(sends, 3)
})

for (const malformedJson of [false, true]) {
  test(`generated client retains HTTP503 attempt facts after ${malformedJson ? 'invalid JSON' : 'domain envelope'} error`, async () => {
    let sends = 0, caught
    const envelope = JSON.stringify({ schemaVersion: 'winwincode/v1', requestId: query.requestId,
      error: { code: 'INTERNAL_ERROR', message: sensitive, retryable: true, details: {} } })
    const client = createControlPlaneHttpClient({ maxNetworkRetries: 2, waitBeforeRetry: async () => {},
      fetch: async () => {
        sends += 1
        return { ...unavailableResponse(malformedJson ? `{${sensitive}` : envelope), headers: { get: name => name === 'retry-after' ? '301' : null } }
      } })
    try { await client.submitQuery(query) } catch (error) { caught = error }
    assert.ok(caught instanceof ControlPlaneClientError)
    assert.equal(sends, 1)
    assert.ok(caught.cause instanceof NetworkError, 'generated non-Error domain exception still needs a request cause')
    safeRequestFacts(retainedRequestFailure(caught), 1, 'deferred')
  })
}

test('generated client wrapping a socket failure keeps its specific inner diagnostic', async () => {
  let sends = 0, caught
  const client = createControlPlaneHttpClient({ maxNetworkRetries: 2,
    waitBeforeRetry: async attempt => { if (attempt === 3) throw new Error('fixture authority ended') },
    fetch: async () => {
      sends += 1
      throw new TypeError(sensitive, { cause: Object.assign(new Error(sensitive), { code: 'ECONNRESET' }) })
    } })
  try { await client.submitQuery(query) } catch (error) { caught = error }
  assert.ok(caught instanceof ControlPlaneClientError)
  assert.equal(sends, 3)
  const facts = retainedRequestFailure(caught)
  assert.equal(facts.network.diagnostic?.ioKind, 'connection_reset')
  assert.equal(facts.attempts.length, 3)
  assert.ok(facts.attempts.every(row => row.failure.diagnostic.ioKind === 'connection_reset'))
  assert.doesNotMatch(JSON.stringify(facts), /SYNTHETIC_PRIVATE|SYNTHETIC_CREDENTIAL|invalid\.example/u)
})

test('safe request projection removes injected fields from nested causes and attempt records', () => {
  const failure = { kind: 'server_transient', acceptance: 'response_received', phase: 'response_headers',
    httpStatus: 503, retryAfterMs: null, url: sensitive,
    diagnostic: { code: 'http_status', message: sensitive, responseLog: sensitive } }
  const source = Object.assign(new Error(sensitive), { failure, networkStopReason: 'retry_budget_exhausted',
    networkAttempts: [{ attempt: 1, networkAttempt: 1, connectionWaits: 0, outcome: 'failed', failure,
      headers: { authorization: sensitive }, payload: sensitive }] })
  const facts = retainedRequestFailure(new Error(sensitive, { cause: source }))
  safeRequestFacts(facts, 1)
  assert.doesNotMatch(JSON.stringify(facts), /SYNTHETIC_PRIVATE|authorization|payload|invalid\.example/u)
})


test('generated domain wrapper prefers specific inner HTTP failure over its generic getter', () => {
  const error = new ControlPlaneClientError({ code: 'NETWORK_ERROR', message: sensitive,
    requestId: query.requestId, retryable: true, details: {} })
  const inner = new NetworkError(httpFailure(503))
  Object.defineProperty(error, 'cause', { value: inner })
  assert.deepEqual(classifyError(error), inner.failure)
})

test('explicit non-retryable schema mismatch cannot become an unlimited protocol retry', () => {
  for (const retryable of [false, true]) {
    const error = new ControlPlaneClientError({ code: 'SCHEMA_VERSION_MISMATCH', message: 'fixture version mismatch',
      requestId: query.requestId, retryable, details: {} })
    assert.equal(decide(classifyError(error), { replay: 'replay_exact' }).action, 'stop')
  }
})

test('exact request preserves generated schema error identity and hides its single attempt metadata', async () => {
  const error = new ControlPlaneClientError({ code: 'SCHEMA_VERSION_MISMATCH', message: 'fixture version mismatch',
    requestId: query.requestId, retryable: true, details: {} })
  let sends = 0, caught
  try {
    await executeRequest(async () => { sends += 1; throw error }, {
      replay: 'replay_exact', waitBeforeRetry: () => assert.fail('permanent schema mismatch cannot retry'),
    })
  } catch (failure) { caught = failure }
  assert.equal(caught, error)
  assert.ok(caught instanceof ControlPlaneClientError)
  assert.equal(caught.code, 'SCHEMA_VERSION_MISMATCH')
  assert.equal(sends, 1)
  assert.equal(caught.networkStopReason, 'permanent_failure')
  assert.equal(caught.networkAttempts.length, 1)
  assert.equal(caught.networkAttempts[0].failure.diagnostic.code, 'schema_version')
  assert.equal(Object.getOwnPropertyDescriptor(caught, 'networkAttempts').enumerable, false)
  assert.equal(Object.getOwnPropertyDescriptor(caught, 'networkStopReason').enumerable, false)
  assert.doesNotMatch(JSON.stringify(caught), /networkAttempts|networkStopReason/u)
})
