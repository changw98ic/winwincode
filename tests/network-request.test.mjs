// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
import { createServer as createHttpsServer } from 'node:https'
import { createCertificate, requestJson } from '../scripts/run-api-production-vertical.mjs'
import { createServer } from 'node:http'
import { once } from 'node:events'
import test from 'node:test'
import { networkEntryErrors } from '../scripts/check-network-entrypoints.mjs'
import { classifyError, decide, executeFetch, executeRequest, httpFailure, NetworkError, retryAfter } from '../packages/network-request/src/index.mjs'

test('Rust and JavaScript consume the same decision vectors', () => {
  const vectors = JSON.parse(readFileSync(new URL('./fixtures/network-request-policy.v1.json', import.meta.url)))
  for (const vector of vectors) {
    assert.deepEqual(decide(vector.failure, vector), {
      action: vector.action, ...(vector.delayMs === undefined ? {} : { delayMs: vector.delayMs }),
    }, vector.name)
  }
  assert.equal(httpFailure(425).kind, 'server_transient')
  assert.equal(httpFailure(401).kind, 'authentication')
  assert.equal(retryAfter('12', 0), 12000)
  assert.equal(retryAfter(new Date(30000).toUTCString(), 10000), 20000)
  assert.equal(classifyError(new TypeError('fetch failed')).acceptance, 'unknown')
})

test('a first model protocol failure retries the same request', async () => {
  const request = Object.freeze({ requestId: 'protocol-retry', model: 'fixture' })
  const requests = [], waits = [], attempts = []
  const failure = { kind: 'protocol_invalid', acceptance: 'unknown', phase: 'response_headers',
    httpStatus: null, retryAfterMs: null }
  const result = await executeRequest(async () => {
    requests.push(request)
    if (requests.length === 1) throw new NetworkError(failure)
    return 'completed'
  }, { replay: 'retry_inference', jitter: () => 0,
    waitBeforeRetry: async (_, delay) => waits.push(delay), onAttempt: fact => attempts.push(fact) })
  assert.equal(result, 'completed')
  assert.deepEqual(requests, [request, request])
  assert.deepEqual(waits, [5000])
  assert.deepEqual(attempts.map(({ attempt, outcome }) => [attempt, outcome]), [[1, 'failed'], [2, 'succeeded']])
})

test('persistent model protocol failures exhaust one bounded retry budget', async () => {
  const attempts = [], waits = []
  await assert.rejects(executeRequest(async ({ attempt }) => {
    attempts.push(attempt)
    throw new NetworkError({ kind: 'protocol_invalid', acceptance: 'response_received', phase: 'decode',
      httpStatus: 200, retryAfterMs: null })
  }, { replay: 'retry_inference', jitter: () => 0,
    waitBeforeRetry: async (_, delay) => waits.push(delay) }), NetworkError)
  assert.deepEqual(attempts, [1, 2, 3, 4])
  assert.deepEqual(waits, [5000, 10000, 20000])
})

test('a real truncated response retries the exact body with one budget', async () => {
  const bodies = [], waits = [], attempts = []
  const server = createServer(async (request, response) => {
    let body = ''
    for await (const bytes of request) body += bytes
    bodies.push(body)
    if (bodies.length === 1) {
      response.writeHead(200, { 'content-length': '99' })
      response.write('{')
      setImmediate(() => response.destroy())
    } else response.end('{"ok":true}')
  })
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  try {
    const response = await executeFetch(fetch, `http://127.0.0.1:${server.address().port}/`, {
      method: 'POST', body: '{"requestId":"retained-request"}',
    }, { replay: 'replay_exact', waitBeforeRetry: async (_, delay) => waits.push(delay), jitter: () => 0,
      onAttempt: fact => attempts.push(fact) })
    assert.equal(await response.text(), '{"ok":true}')
    assert.deepEqual(bodies, ['{"requestId":"retained-request"}', '{"requestId":"retained-request"}'])
    assert.deepEqual(waits, [5000])
    assert.equal(attempts[0].failure.acceptance, 'unknown')
  } finally { server.closeAllConnections(); server.close(); await once(server, 'close') }
})

test('unknown mutation is not sent twice', async () => {
  let calls = 0
  await assert.rejects(executeRequest(async () => { calls += 1; throw new TypeError('lost response') }, {
    replay: 'reconcile_first', waitBeforeRetry: async () => assert.fail('must reconcile'),
  }))
  assert.equal(calls, 1)
})

test('failed attempt persistence never triggers another network request', async () => {
  let calls = 0
  await assert.rejects(executeRequest(async () => { calls += 1; return 1 }, {
    onAttempt: () => { throw new Error('storage unavailable') },
  }), /storage unavailable/u)
  assert.equal(calls, 1)
})

test('authority revocation aborts active I/O', async () => {
  let active = true, aborted = false
  let started
  const ready = new Promise(resolve => { started = resolve })
  const pending = executeRequest(({ signal }) => new Promise((_, reject) => {
    signal.addEventListener('abort', () => { aborted = true; reject(new DOMException('stopped', 'AbortError')) })
    started()
  }), { canStart: () => active })
  await ready
  active = false
  await assert.rejects(pending, NetworkError)
  assert.equal(aborted, true)
})


test('the Node HTTPS entry retries HTML 503 and retains the exact query body', async () => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-network-https-'))
  const certificate = createCertificate(directory)
  const bodies = []
  const server = createHttpsServer({ key: readFileSync(certificate.key), cert: readFileSync(certificate.cert) }, async (request, response) => {
    let body = ''
    for await (const bytes of request) body += bytes
    bodies.push(body)
    if (bodies.length === 1) { response.writeHead(503); response.end('<html>temporary</html>') }
    else response.end('{"ok":true}')
  })
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  try {
    const response = await requestJson(`https://127.0.0.1:${server.address().port}/api/v1/queries`, { method: 'POST', origin: 'https://control.localhost', ca: readFileSync(certificate.cert), body: { requestId: 'fixed' }, timeoutMillis: 15000 })
    assert.equal(response.status, 200)
    assert.deepEqual(response.json, { ok: true })
    assert.equal(bodies.length, 2)
    assert.equal(bodies[0], bodies[1])
  } finally { server.closeAllConnections(); server.close(); await once(server, 'close'); rmSync(directory, { recursive: true, force: true }) }
})

test('an idempotent occupancy claim retries 503 with the same holder and client', async () => {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-occupancy-https-'))
  const certificate = createCertificate(directory)
  const bodies = []
  const server = createHttpsServer({ key: readFileSync(certificate.key), cert: readFileSync(certificate.cert) }, async (request, response) => {
    let body = ''
    for await (const bytes of request) body += bytes
    bodies.push(body)
    if (bodies.length === 1) {
      response.writeHead(503)
      response.end('{"error":{"code":"SERVICE_UNAVAILABLE","retryable":true}}')
    } else {
      response.writeHead(201)
      response.end('{"occupancy":"occupied","holderUserId":"fixture-holder"}')
    }
  })
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  try {
    const response = await requestJson(`https://127.0.0.1:${server.address().port}/api/v1/clients/occupancy`, {
      method: 'POST', cookie: 'fixture-holder', origin: 'https://control.localhost', ca: readFileSync(certificate.cert),
      body: { schemaVersion: 'winwincode/v1', clientId: 'fixture-client' },
      timeoutMillis: 15000,
    })
    assert.equal(response.status, 201)
    assert.equal(bodies.length, 2)
    assert.equal(bodies[0], bodies[1])
  } finally { server.closeAllConnections(); server.close(); await once(server, 'close'); rmSync(directory, { recursive: true, force: true }) }
})


test('the network source gate rejects a new unregistered raw transport', () => {
  const directory = mkdtempSync(join(tmpdir(), 'network-entry-gate-'))
  try {
    const inventory = JSON.parse(readFileSync(new URL('../scripts/network-entrypoints.json', import.meta.url)))
    for (const name of ['crates', 'apps', 'packages', 'scripts']) mkdirSync(join(directory, name))
    for (const entry of inventory.entries) {
      const path = join(directory, entry.file)
      mkdirSync(join(path, '..'), { recursive: true })
      writeFileSync(path, [...entry.operations, ...entry.policyMarkers].join('\n'))
    }
    assert.deepEqual(networkEntryErrors(directory), [])
    writeFileSync(join(directory, 'crates/unregistered.rs'), 'reqwest::Client::new()')
    assert.deepEqual(networkEntryErrors(directory), ['crates/unregistered.rs: unregistered native network entry'])
  } finally { rmSync(directory, { recursive: true, force: true }) }
})
