import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.settings-tests.json',
    '--pretty',
    'false',
    '--incremental',
    'false',
  ],
  { cwd: root, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `Settings client did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const facade = await import(`${pathToFileURL(resolve(
  root,
  '.cache/settings-tests/community-control-plane-client.js',
)).href}`)
const settingsModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/settings-tests/settings-view-model.js',
)).href}`)
const pageModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/settings-tests/settings-page.js',
)).href}`)

const { createControlPlaneClient } = facade
const { createSettingsViewModel } = settingsModule
const { mountSettingsPage, settingsPagePresentation } = pageModule
const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const subscriptionId = 'sub_00000000000000000000000001'
function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function requestId(value) {
  return canonicalId('req', value)
}

function eventId(value) {
  return canonicalId('evt', value)
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function response(status, payload) {
  return {
    ok: status >= 200 && status < 300,
    status,
    async text() {
      return JSON.stringify(payload)
    },
  }
}

function queryResponse(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: page(),
  }
}

function commandResponse(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    command: request.command,
    outcome: 'completed',
    previousRevision: request.expectedRevision,
    currentRevision: result.revision,
    result,
  }
}

function terminalError(request, code, message) {
  return {
    schemaVersion,
    requestId: request.requestId,
    error: { code, message, retryable: false, details: {} },
  }
}

function scopeCursor(sequence = 0) {
  return {
    scope,
    stream: { kind: 'scope' },
    sequence,
    eventId: sequence === 0 ? null : eventId(sequence),
  }
}

function transportLimits() {
  return {
    maxUnackedEvents: 256,
    hardUnackedEvents: 1024,
    ackDeadlineMillis: 30_000,
    backpressureCloseCode: 4408,
  }
}

class FakeWebSocket {
  readyState = 0
  onopen = null
  onmessage = null
  onclose = null
  onerror = null
  sent = []

  send(source) {
    assert.equal(this.readyState, 1)
    this.sent.push(JSON.parse(source))
  }

  close() {
    this.readyState = 3
  }

  open() {
    this.readyState = 1
    this.onopen?.({})
  }

  receive(frame) {
    this.onmessage?.({ data: JSON.stringify(frame) })
  }
}

function contractFake({ deferred = false } = {}) {
  const requests = [], sockets = []
  let settings = { revision: 1, defaultModelRoute: null, workerConcurrencyLimit: 2 }
  let pending
  return { requests, sockets,
    async fetch(input, init) {
      const request = JSON.parse(init.body)
      requests.push({input, request})
      if (request.query === 'settings.get') return response(200, queryResponse(request, settings))
      assert.equal(request.command, 'settings.update')
      assert.equal(request.expectedRevision, settings.revision)
      pending = { ...request.payload.patch, revision: settings.revision + 1 }
      if (deferred) return response(200, {schemaVersion,requestId:request.requestId,command:request.command,outcome:'accepted',acceptedAt:'2026-08-27T01:00:01.000Z',currentRevision:settings.revision})
      settings = pending
      return response(200, commandResponse(request, settings))
    },
    createSocket() { const socket = new FakeWebSocket(); sockets.push(socket); return socket },
    settle() { settings = pending },
  }
}
function setup(fake, selectedScope = scope) {
  const client = createControlPlaneClient({serverUrl:'https://control.example',maxNetworkRetries:0,transport:{fetch:fake.fetch,createSocket:fake.createSocket}})
  let next = 0
  const model = createSettingsViewModel({client,actor,scope:selectedScope,subscriptionId,nextRequestId:()=>requestId(++next)})
  return {client,model}
}
test('settings reads only shared concurrency and saves using the observed revision', async()=>{
  const fake=contractFake(), {client,model}=setup(fake)
  await model.start()
  assert.deepEqual(fake.requests.map(x=>x.request.query),['settings.get'])
  await model.updateSettings({workerConcurrencyLimit:4})
  assert.deepEqual(fake.requests.at(-1).request.payload.patch,{defaultModelRoute:null,workerConcurrencyLimit:4})
  assert.equal(model.state.settings.revision,2)
  assert.equal(model.state.settings.workerConcurrencyLimit,4)
  const count=fake.requests.length
  await model.updateSettings({workerConcurrencyLimit:0})
  assert.equal(fake.requests.length,count)
  assert.equal(model.state.interaction.error.code,'SETTINGS_CONCURRENCY_INVALID')
  model.close();client.close()
})
test('accepted concurrency writes wait for refreshed durable state and prevent duplicate submission', async()=>{
  const fake=contractFake({deferred:true}),{client,model}=setup(fake)
  await model.start();await model.updateSettings({workerConcurrencyLimit:4})
  assert.equal(model.state.interaction.status,'waiting')
  assert.equal(model.state.settings.workerConcurrencyLimit,2)
  await model.updateSettings({workerConcurrencyLimit:5})
  assert.equal(fake.requests.filter(x=>x.request.command).length,1)
  fake.settle();await model.refresh()
  assert.equal(model.state.settings.workerConcurrencyLimit,4)
  model.close();client.close()
})
test('invalid scope and access errors remain bounded before requests reach the server', async()=>{
  const fake=contractFake(),{client,model}=setup(fake,{...scope,repositoryId:'invalid'})
  await model.start()
  assert.equal(fake.requests.length,0)
  assert.equal(model.state.error.code,'INVALID_CLIENT_REQUEST')
  assert.equal(settingsPagePresentation(model.state).errorText,'请检查本地用户身份和工作区范围配置后重试。')
  model.close();client.close()
})
