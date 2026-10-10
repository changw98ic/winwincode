// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import test from 'node:test'
import { createECDH } from 'node:crypto'
import { once } from 'node:events'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { createServer } from 'node:https'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { execFile } from 'node:child_process'
import { promisify } from 'node:util'
import { ApiClient, createCertificate, requestJson } from '../scripts/run-api-production-vertical.mjs'
import { encryptDeviceProviderEnvelope, installDevicePublicSmoke, seedDeviceLocalProvider } from '../scripts/device-production-fixture.mjs'

function snapshot() {
  const key = createECDH('prime256v1')
  key.generateKeys()
  return { clientNodeId: 'cix_fixture', revision: 1,
    encryptionPublicKey: key.getPublicKey().toString('base64') }
}

for (const [status, code] of [[409, 'REVISION_CONFLICT'], [503, 'DEVICE_UNAVAILABLE']]) {
  test(`public smoke apply preserves safe rejection identity for HTTP ${status}`, async () => {
    const requests = [], secret = 'synthetic-private-configuration-message'
    const current = snapshot()
    const api = { async request(path, options) {
      requests.push({ path, options })
      if (!options) return { status: 200, json: { online: true, snapshot: current } }
      assert.equal(options.method, 'POST')
      return { status, json: { error: { code, message: secret } } }
    } }
    await assert.rejects(installDevicePublicSmoke({ api, publicClientId: 'fixture',
      configuration: { command: '/fixture/public-smoke', args: [] } }), error => {
      assert.equal(error.code, code)
      assert.equal(error.status, status)
      assert.equal(error.phase, 'device_configuration_apply')
      assert.equal(error.requestId, requests[1].options.body.requestId)
      assert.equal(error.message.includes(secret), false)
      assert.equal(JSON.stringify(error).includes(secret), false)
      return true
    })
    assert.equal(requests.length, 2)
    assert.equal(requests[1].options.body.ciphertext.includes(secret), false)
  })
}

async function httpsFixture(t, respond) {
  const directory = mkdtempSync(join(tmpdir(), 'wwc-device-configuration-'))
  const certificate = createCertificate(directory)
  const bodies = []
  const server = createServer({ key: readFileSync(certificate.key),
    cert: readFileSync(certificate.cert) }, async (request, response) => {
    let body = ''
    for await (const bytes of request) body += bytes
    bodies.push({ path: request.url, method: request.method, body })
    respond(request, response, bodies)
  })
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  t.after(async () => {
    server.closeAllConnections()
    server.close()
    await once(server, 'close')
    rmSync(directory, { recursive: true, force: true })
  })
  return { bodies, certificate: certificate.cert, url: `https://127.0.0.1:${server.address().port}`,
    options: { method: 'POST', origin: 'https://control.localhost',
      ca: readFileSync(certificate.cert), timeoutMillis: 20_000 } }
}

function envelope() {
  return encryptDeviceProviderEnvelope(snapshot(), 'extension_retained_fixture',
    { operation: 'delete', kind: 'mcp', id: 'benchmark_public_smoke' })
}

test('encrypted Device configuration applies retry 503 with exact retained ciphertext', async t => {
  const counts = new Map()
  const fixture = await httpsFixture(t, (request, response) => {
    const count = (counts.get(request.url) ?? 0) + 1
    counts.set(request.url, count)
    response.writeHead(count === 1 ? 503 : 202, { 'content-type': 'application/json' })
    response.end(JSON.stringify(count === 1 ? { error: { code: 'DEVICE_UNAVAILABLE' } }
      : { requestId: 'extension_retained_fixture', status: 'waiting' }))
  })
  const body = envelope()
  await Promise.all(['extensions', 'providers', 'repositories'].map(async kind => {
    const path = `/api/v1/clients/fixture/${kind}`
    const response = await requestJson(`${fixture.url}${path}`, { ...fixture.options, body })
    assert.equal(response.status, 202, kind)
    const sent = fixture.bodies.filter(row => row.path === path)
    assert.equal(sent.length, 2, kind)
    assert.equal(sent[0].body, sent[1].body)
    assert.equal(JSON.parse(sent[0].body).requestId, body.requestId)
  }))
})

test('deferred encrypted apply returns its safe HTTP failure and attempt history', async t => {
  const fixture = await httpsFixture(t, (_, response) => {
    response.writeHead(503, { 'retry-after': '301', 'content-type': 'application/json' })
    response.end('{"error":{"code":"DEVICE_UNAVAILABLE"}}')
  })
  const response = await requestJson(`${fixture.url}/api/v1/clients/fixture/extensions`,
    { ...fixture.options, body: envelope() })
  assert.equal(response.status, 503)
  assert.equal(fixture.bodies.length, 1)
  assert.equal(response.networkError?.failure.httpStatus, 503)
  assert.equal(response.networkError.networkAttempts.length, 1)
  assert.equal(response.networkError.networkStopReason, 'deferred')
  assert.equal(Object.keys(response).includes('networkError'), false)
})

test('ordinary mutations and incomplete configuration envelopes require reconciliation', async t => {
  const fixture = await httpsFixture(t, (_, response) => {
    response.writeHead(503)
    response.end('{"error":{"code":"DEVICE_UNAVAILABLE"}}')
  })
  for (const [path, body] of [['/api/v1/ordinary-mutation', envelope()],
    ['/api/v1/clients/fixture/extensions', { expectedRevision: 1 }],
    ['/api/v1/clients/fixture/extensions', { ...envelope(), ciphertext: '' }],
    ['/api/v1/clients/fixture/extensions', { ...envelope(), requestId: 'x'.repeat(7) }],
    ['/api/v1/clients/fixture/extensions', { ...envelope(), requestId: 'x'.repeat(201) }],
    ['/api/v1/clients/fixture/extensions', { ...envelope(), requestId: 'invalid\nidentity' }]]) {
    const before = fixture.bodies.length
    const response = await requestJson(`${fixture.url}${path}`, { ...fixture.options, body })
    assert.equal(response.status, 503)
    assert.equal(fixture.bodies.length - before, 1)
  }
})

test('encrypted apply never retries permanent HTTP authentication and conflict failures', async t => {
  for (const status of [401, 403, 409]) {
    const fixture = await httpsFixture(t, (_, response) => {
      response.writeHead(status)
      response.end('{"error":{"code":"FIXTURE_REJECTION"}}')
    })
    const response = await requestJson(`${fixture.url}/api/v1/clients/fixture/extensions`,
      { ...fixture.options, body: envelope() })
    assert.equal(response.status, status)
    assert.equal(fixture.bodies.length, 1)
  }
})

test('encrypted apply continues beyond four failures and admits the server request ID boundaries', async t => {
  const fixture = await httpsFixture(t, (_, response, bodies) => {
    const id = JSON.parse(bodies.at(-1).body).requestId
    const attempts = bodies.filter(row => JSON.parse(row.body).requestId === id).length
    response.writeHead(attempts <= 5 ? 503 : 202)
    response.end('{"status":"waiting"}')
  })
  // The executor and TLS transport remain real. Its public waiter seam removes
  // wall-clock backoff from this six-attempt policy regression in a child only.
  const child = `
    import assert from 'node:assert/strict';
    import { mock } from 'node:test';
    import { readFileSync } from 'node:fs';
    const url = ${JSON.stringify(new URL('../packages/network-request/src/index.mjs', import.meta.url).href)};
    const network = await import(url);
    const waits = [];
    mock.module(url, { namedExports: { ...network,
      executeRequest: (attempt, options) => network.executeRequest(attempt,
        { ...options, jitter: () => 0, waitBeforeRetry: async (_, delay) => waits.push(delay) }) } });
    const { requestJson } = await import(${JSON.stringify(new URL('../scripts/run-api-production-vertical.mjs', import.meta.url).href)});
    const response = await requestJson(process.argv[1], { method: 'POST', origin: 'https://control.localhost',
      ca: readFileSync(process.argv[2]), body: JSON.parse(process.argv[3]), timeoutMillis: 20000 });
    assert.equal(response.status, 202);
    assert.equal(waits.length, 5);
    assert.equal(waits.every(delay => delay >= 5000), true);
    process.stdout.write(JSON.stringify({ status: response.status, waits }));
  `
  for (const length of [8, 160, 200]) {
    const body = { ...envelope(), requestId: 'x'.repeat(length) }
    const { stdout } = await promisify(execFile)(process.execPath,
      ['--experimental-test-module-mocks', '--input-type=module', '-e', child,
        `${fixture.url}/api/v1/clients/fixture/extensions`, fixture.certificate, JSON.stringify(body)])
    assert.equal(JSON.parse(stdout).status, 202)
    const sent = fixture.bodies.filter(row => JSON.parse(row.body).requestId === body.requestId)
    assert.equal(sent.length, 6)
    assert.equal(sent.every(row => row.body === sent[0].body), true)
  }
})

test('public smoke rejection inherits safe network history without exposing response message', async t => {
  const fixture = await httpsFixture(t, (_, response) => {
    response.writeHead(503, { 'retry-after': '301' })
    response.end('{"error":{"code":"DEVICE_UNAVAILABLE","message":"synthetic-private-response"}}')
  })
  const current = snapshot()
  const api = { async request(path, options) {
    if (!options) return { status: 200, json: { online: true, snapshot: current } }
    return requestJson(`${fixture.url}${path}`, { ...fixture.options, ...options })
  } }
  await assert.rejects(installDevicePublicSmoke({ api, publicClientId: 'fixture', configuration: {} }), error => {
    assert.equal(error.code, 'DEVICE_UNAVAILABLE')
    assert.equal(error.status, 503)
    assert.equal(error.phase, 'device_configuration_apply')
    assert.equal(error.requestId, JSON.parse(fixture.bodies[0].body).requestId)
    assert.equal(error.cause.failure.httpStatus, 503)
    assert.deepEqual(error.networkAttempts, error.cause.networkAttempts)
    assert.equal(error.networkStopReason, 'deferred')
    assert.equal(Object.keys(error).includes('cause'), false)
    assert.equal(error.message.includes('synthetic-private-response'), false)
    assert.equal(JSON.stringify(error).includes('synthetic-private-response'), false)
    return true
  })
  assert.equal(fixture.bodies.length, 1)
})

test('public smoke installation waits for both saved and tested receipts', async () => {
  const current = snapshot(), requests = [], receipts = new Map()
  const api = { async request(path, options) {
    requests.push({ path, options })
    if (options?.method === 'POST') {
      const body = options.body
      assert.ok(body.ciphertext.length > 0)
      assert.equal(body.expectedRevision, current.revision)
      assert.equal(body.clientNodeId, current.clientNodeId)
      const outcome = receipts.size === 0 ? 'saved' : 'tested'
      receipts.set(body.requestId, { requestId: body.requestId, outcome })
      return { status: 202, json: { requestId: body.requestId, status: 'waiting' } }
    }
    if (path.includes('/receipts/')) return { status: 200, json: {
      receipt: receipts.get(path.split('/').at(-1)), snapshot: { ...current,
        mcpServers: [{ id: 'benchmark_public_smoke', enabled: true,
          connectionStatus: 'ready', toolNames: ['public_smoke'] }] } } }
    return { status: 200, json: { online: true, snapshot: current } }
  } }
  const result = await installDevicePublicSmoke({ api, publicClientId: 'fixture', configuration: {} })
  assert.deepEqual(result.receipts.map(receipt => receipt.outcome), ['saved', 'tested'])
  assert.deepEqual(result.toolNames, ['public_smoke'])
  assert.equal(requests.filter(request => request.options?.method === 'POST').length, 2)
  assert.equal(receipts.size, 2)
})

test('Provider seed rejection preserves safe apply facts without exposing response text', async () => {
  const requests = [], secret = 'synthetic-private-provider-response'
  const api = { async request(path, options) {
    requests.push({ path, options })
    if (!options) return { status: 200, json: { snapshot: snapshot() } }
    return { status: 409, text: secret, json: { error: { code: 'REVISION_CONFLICT', message: secret } } }
  } }
  await assert.rejects(seedDeviceLocalProvider({ api, publicClientId: 'fixture',
    providerId: 'local-fixture', modelId: 'local-fixture', endpoint: 'https://provider.invalid/v1/messages',
    apiKey: 'opaque-fixture-key' }), error => {
    assert.equal(error.message.includes(secret), false)
    assert.equal(error.code, 'REVISION_CONFLICT')
    assert.equal(error.status, 409)
    assert.equal(error.phase, 'device_configuration_apply')
    assert.equal(error.requestId, requests[1].options.body.requestId)
    assert.equal(JSON.stringify(error).includes(secret), false)
    return true
  })
  assert.equal(requests.length, 2)
  assert.equal(requests[1].options.body.ciphertext.includes('opaque-fixture-key'), false)
})

for (const operation of ['query', 'command']) {
  test(`API ${operation} wrapper preserves safe HTTP history and omits response message`, async t => {
    const fixture = await httpsFixture(t, (_, response) => {
      response.writeHead(503, { 'retry-after': '301' })
      response.end('{"error":{"code":"DEVICE_UNAVAILABLE","message":"synthetic-private-api-response"}}')
    })
    const api = new ApiClient(fixture.url, fixture.options.origin, 1, fixture.options.ca)
    await assert.rejects(operation === 'query' ? api.query('workrun.get', {})
      : api.command('workrun.start', 1, {}), error => {
      assert.equal(error.message.includes('synthetic-private-api-response'), false)
      assert.equal(error.code, 'DEVICE_UNAVAILABLE')
      assert.equal(error.status, 503)
      assert.equal(error.requestId, JSON.parse(fixture.bodies[0].body).requestId)
      assert.equal(error.cause.failure.httpStatus, 503)
      assert.equal(error.networkAttempts.length, 1)
      assert.equal(error.networkStopReason, 'deferred')
      assert.equal(Object.keys(error).includes('cause'), false)
      assert.equal(JSON.stringify(error).includes('synthetic-private-api-response'), false)
      return true
    })
    assert.equal(fixture.bodies.length, 1)
  })
}
