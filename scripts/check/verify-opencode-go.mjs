// SPDX-License-Identifier: Apache-2.0

// Explicit live acceptance. Run once per output directory; paid receipts are retained.
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash, randomUUID } from 'node:crypto'
import { globSync, mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { configuredDeviceModelRoute, encryptDeviceProviderEnvelope, runApiProductionVertical,
  waitForDeviceWorkerRegistered } from '../acceptance/run-api-production-vertical.mjs'
import { waitFor } from '../lib/device-production-fixture.mjs'
import { readDeviceExecutionReceipts } from '../acceptance/export-device-candidate.mjs'

const resumeBeforeCalls = process.argv.includes('--resume-before-calls')
const recoverCompleted = process.argv.includes('--recover-completed')
assert.ok(!(resumeBeforeCalls && recoverCompleted), 'choose one recovery phase')
const [output, authA, orgA, authB, orgB] = process.argv.slice(2).filter(argument => !['--resume-before-calls', '--recover-completed'].includes(argument))
assert.ok(output && authA && orgA && authB && orgB, 'requires output, authorization A, org A, authorization B, org B')
const directory = resolve(output)
assert.ok(!directory.startsWith(`${resolve(import.meta.dirname, '../..')}/`), 'live evidence must stay outside the checkout')
if (!resumeBeforeCalls && !recoverCompleted) mkdirSync(directory, { mode: 0o700 })
else {
  const db = new DatabaseSync(join(directory, 'device-data/providers/providers.sqlite3'), { readOnly: true })
  const count = db.prepare('SELECT count(*) AS count, count(chunks) AS completed FROM exchanges').get()
  if (resumeBeforeCalls) assert.equal(count.count, 0, 'resume only before any model call')
  else {
    const calls = JSON.parse(readFileSync(join(directory, 'calls.json')))
    assert.ok(calls.length > 0 && calls.every(call => call.terminalType === 'completed'))
    assert.equal(count.count, calls.length)
    assert.equal(count.completed, calls.length, 'recovery requires every original call to be completed')
  }
  db.close()
}
const root = resolve(import.meta.dirname, '../..')
const probe = resolve(process.env.CARGO_TARGET_DIR ?? join(root, 'target'), 'debug/examples/opencode_go_probe')
const sessions = ['psn_01J00000000000000000000071', 'psn_01J00000000000000000000072']
const marker = resumeBeforeCalls || recoverCompleted
  ? readFileSync(join(directory, 'source-repositories/rep_01J00000000000000000000000/TASK.md'), 'utf8').trim()
  : `OC-GO-${randomUUID()}`
const usageBeforeFile = `usage-before-${randomUUID()}.json`
const decode = payload => JSON.parse(Buffer.from(payload.dataBase64, 'base64'))
const retain = (name, value) => writeFileSync(join(directory, name), `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600, flag: 'wx' })
const placeholder = { providerId: 'oauth-provisioning-unused', modelId: 'unused', displayName: 'Provisioning fixture',
  endpoint: 'https://opencode.ai/inference/go/openai/v1/chat/completions', protocol: 'openai_chat_completions' }
const options = { directory, retainRepository: true, restart: false, repeat: false,
  deviceProvider: placeholder, deviceProviderSecrets: ['unused-fixture-secret'],
  deviceAgentEnvironment: { WWC_WORKER_MODEL_REASONING_EFFORT: 'low' } }
let bindings; let retainedCalls; let retainedMessages

async function controls(api, client, command, outcomes = ['saved']) {
  const path = `/api/v1/clients/${encodeURIComponent(client)}/providers`
  const snapshot = (await api.request(path)).json.snapshot
  const requestId = `oauth_${randomUUID().replaceAll('-', '')}`
  const envelope = encryptDeviceProviderEnvelope(snapshot, requestId, command)
  assert.equal((await api.request(path, { method: 'POST', body: envelope })).status, 202)
  const result = await waitFor(async () => {
    const result = (await api.request(`${path}/receipts/${requestId}`)).json
    return result.receipt === null ? false : result
  }, 'encrypted OAuth command receipt', 60_000)
  assert.ok(outcomes.includes(result.receipt.outcome), 'OAuth control must complete')
  return result.snapshot
}
function readStore(devicePath, query) {
  const db = new DatabaseSync(join(devicePath.deviceData, 'providers/providers.sqlite3'), { readOnly: true })
  try { db.exec('PRAGMA busy_timeout=5000'); return query(db) } finally { db.close() }
}
function toolLoops(devicePath) {
  return readStore(devicePath, db => ['go-account-a', 'go-account-b'].map(provider => {
    const rows = db.prepare('SELECT request_open, chunks FROM exchanges WHERE request_open IS NOT NULL ORDER BY rowid').all()
      .map(row => ({ request: decode(JSON.parse(row.request_open).request), chunks: JSON.parse(row.chunks ?? '[]') }))
      .filter(row => row.request.provider === provider)
    const frames = rows.flatMap(row => row.chunks.filter(chunk => chunk.payload).map(chunk => decode(chunk.payload)))
    const calls = frames.filter(frame => frame.type === 'output_item_done' && ['function_call', 'custom_tool_call'].includes(frame.item?.type))
    const output = rows.some(row => JSON.stringify(row.request.request.input).includes(marker)
      && row.request.request.input.some(item => ['function_call_output', 'custom_tool_call_output'].includes(item.type)))
    assert.ok(calls.length > 0 && output, `${provider} must retain native tool call and returned marker`)
    return { provider, toolCalls: calls.length, returnedMarker: output }
  }))
}
async function run() {
  const first = recoverCompleted ? JSON.parse(readFileSync(join(directory, 'first.json'))) : await runApiProductionVertical({ ...options, build: true, scenario: {
    files: { 'TASK.md': `${marker}\n` },
    async run({ api, devicePath }) {
      const store = join(devicePath.deviceData, 'providers')
      if (!resumeBeforeCalls) {
        for (const [auth, org, provider] of [[authA, orgA, 'go-account-a'], [authB, orgB, 'go-account-b']]) {
          const identity = JSON.parse(execFileSync(probe, ['import-auth', store, resolve(auth), provider, org], { encoding: 'utf8' }))
          retain(`${provider}.identity.json`, identity)
        }
      }
      // Private fixture provisioning does not emit a Device report. A harmless
      // stale-revision command publishes the current snapshot without sending HTTP.
      await controls(api, devicePath.publicClientId, { operation: 'set_default_provider', providerId: 'go-account-a' }, ['saved', 'revision_conflict'])
      const snapshot = await waitFor(async () => {
        const result = (await api.request(`/api/v1/clients/${devicePath.publicClientId}/providers`)).json.snapshot
        return result?.openCodeAccounts?.length === 2 ? result : false
      }, 'two native account projections', 60_000)
      assert.notEqual(snapshot.openCodeAccounts[0].subject, snapshot.openCodeAccounts[1].subject)
      const before = []
      for (const [index, providerId] of ['go-account-a', 'go-account-b'].entries()) {
        const accountRef = snapshot.providers.find(provider => provider.config.providerId === providerId).openCode.accountRef
        const measured = await controls(api, devicePath.publicClientId, { operation: 'opencode_usage', accountRef, organizationId: index === 0 ? orgA : orgB })
        before.push(measured.openCodeAccounts.find(account => account.accountRef === accountRef).usage)
      }
      retain(usageBeforeFile, before)
      const routes = ['go-account-a', 'go-account-b'].map(providerId => configuredDeviceModelRoute({ clientNodeId: snapshot.clientNodeId, providerId, modelId: 'glm-5.3-flash' }))
      for (const providerId of ['go-account-a', 'go-account-b']) {
        await controls(api, devicePath.publicClientId, { operation: 'set_default_provider', providerId })
        const index = providerId === 'go-account-a' ? 0 : 1
        const available = (await api.query('model.route.availability.list', {})).result
        assert.equal(available.defaultProviderId, providerId)
        let existing = null
        try { existing = (await api.query('session.get', { productSessionId: sessions[index] })).result }
        catch (error) { if (error.code !== 'RESOURCE_NOT_FOUND') throw error }
        if (existing === null) {
          const created = await api.command('session.create', 0, { productSessionId: sessions[index], projectId: 'prj_01J00000000000000000000000',
            repositoryId: 'rep_01J00000000000000000000000', title: `OpenCode account ${index}`, modelRoute: routes[index] })
          assert.equal(created.outcome, 'completed')
        }
        assert.equal((await api.query('model.route.availability.list', { productSessionId: sessions[index] })).result.defaultProviderId, providerId)
      }
      const old = (await api.query('model.route.availability.list', { productSessionId: sessions[0] })).result
      assert.equal(old.defaultProviderId, 'go-account-a', 'new default must preserve existing Session route')
      // Configuration can exceed the existing idle occupancy window. Claim through
      // the normal public boundary immediately before starting native Workers.
      const occupancy = await waitFor(async () => {
        const result = (await api.request(`/api/v1/clients/${devicePath.publicClientId}/occupancy`)).json
        return ['available', 'occupied'].includes(result.occupancy) ? result : false
      }, 'idle Device occupancy settlement', 60_000)
      if (occupancy.occupancy === 'available') {
        assert.equal((await api.request('/api/v1/clients/occupancy', { method: 'POST',
          body: { schemaVersion: 'winwincode/v1', clientId: devicePath.publicClientId } })).status, 201)
      } else assert.equal(occupancy.holderUserId, api.actor.id)
      const launches = await Promise.all(sessions.map(async productSessionId => {
        const launched = await devicePath.forProductSession(productSessionId).launchAnchor({ productSessionId })
        await waitForDeviceWorkerRegistered(api, launched, 60_000); return launched
      }))
      for (const productSessionId of sessions) {
        const session = (await api.query('session.get', { productSessionId })).result
        const submitted = await api.command('chat.submit', session.revision, { productSessionId,
          message: 'Use the provided terminal tool to execute cat TASK.md. Reply with the exact marker read from TASK.md. Call the tool before answering.' })
        assert.equal(submitted.outcome, 'completed')
      }
      let concurrent = false
      retainedMessages = await waitFor(async () => {
        concurrent ||= readStore(devicePath, db => {
          const providers = db.prepare('SELECT request_open FROM exchanges WHERE chunks IS NULL AND request_open IS NOT NULL').all()
            .map(row => decode(JSON.parse(row.request_open).request).provider)
          return providers.includes('go-account-a') && providers.includes('go-account-b')
        })
        const messages = await Promise.all(sessions.map(async productSessionId => (await api.query('session.messages.list', { productSessionId })).result.items))
        return messages.every(items => items.some(item => item.role === 'assistant' && item.state === 'completed' && item.content.includes(marker))) ? messages : false
      }, 'two real Worker tool loops', 240_000, 100)
      assert.equal(concurrent, true, 'both native exchanges must be in flight at once')
      const loops = toolLoops(devicePath)
      bindings = readStore(devicePath, db => db.prepare('SELECT binding FROM opencode_session_bindings ORDER BY product_session_id').all().map(row => JSON.parse(row.binding)))
      assert.equal(bindings.length, 2); assert.notEqual(bindings[0].accountRef, bindings[1].accountRef)
      assert.notEqual(bindings[0].conversationId, bindings[1].conversationId)
      const after = []
      for (const [index, providerId] of ['go-account-a', 'go-account-b'].entries()) {
        const account = snapshot.providers.find(provider => provider.config.providerId === providerId).openCode.accountRef
        const updated = await controls(api, devicePath.publicClientId, { operation: 'opencode_usage', accountRef: account, organizationId: index === 0 ? orgA : orgB })
        assert.ok(updated.openCodeAccounts.find(value => value.accountRef === account).usage?.updatedAtMs > 0)
        after.push(updated.openCodeAccounts.find(value => value.accountRef === account).usage)
      }
      retain('usage-after.json', after)
      retainedCalls = readDeviceExecutionReceipts(directory).calls
      assert.ok(retainedCalls.every(call => call.terminalType === 'completed' && call.usage?.totalTokens > 0))
      retain('calls.json', retainedCalls); retain('bindings.json', bindings)
      retain('messages.json', retainedMessages)
      return { concurrent, loops, usageBeforeFile, defaultSwitchPreserved: true, launches: launches.map(launch => launch.workerSessionId), calls: retainedCalls.length }
    },
  } })
  if (!recoverCompleted) retain('first.json', first)
  else {
    assert.equal(first.flow.scenario.concurrent, true)
    assert.ok(first.flow.scenario.loops.every(loop => loop.toolCalls > 0 && loop.returnedMarker))
    bindings = JSON.parse(readFileSync(join(directory, 'bindings.json')))
    retainedCalls = JSON.parse(readFileSync(join(directory, 'calls.json')))
    const baseline = await runApiProductionVertical({ ...options, build: true, scenario: {
      files: { 'TASK.md': `${marker}\n` },
      async run({ api, devicePath }) {
        retainedMessages = await Promise.all(sessions.map(async productSessionId => (await api.query('session.messages.list', { productSessionId })).result.items))
        assert.ok(retainedMessages.every(items => items.some(item => item.role === 'assistant' && item.state === 'completed' && item.content.includes(marker))))
        assert.deepEqual(readDeviceExecutionReceipts(directory).calls, retainedCalls)
        assert.deepEqual(readStore(devicePath, db => db.prepare('SELECT binding FROM opencode_session_bindings ORDER BY product_session_id').all().map(row => JSON.parse(row.binding))), bindings)
        retain('messages-recovery.json', retainedMessages)
        return { originalPaidCalls: retainedCalls.length, messagesCompleted: true, modelCallsStarted: 0 }
      },
    } })
    retain('recovery-baseline.json', baseline)
  }
  const restored = await runApiProductionVertical({ ...options, build: false, scenario: {
    files: { 'TASK.md': `${marker}\n` },
    async run({ api, devicePath }) {
      for (const [index, productSessionId] of sessions.entries()) {
        const messages = (await api.query('session.messages.list', { productSessionId })).result.items
        assert.deepEqual(messages, retainedMessages[index])
        assert.equal((await api.query('model.route.availability.list', { productSessionId })).result.defaultProviderId, index === 0 ? 'go-account-a' : 'go-account-b')
      }
      assert.deepEqual(readStore(devicePath, db => db.prepare('SELECT binding FROM opencode_session_bindings ORDER BY product_session_id').all().map(row => JSON.parse(row.binding))), bindings)
      assert.deepEqual(readDeviceExecutionReceipts(directory).calls, retainedCalls)
      // Recover each completed native exchange through the real Device replay boundary.
      const opens = readStore(devicePath, db => db.prepare('SELECT exchange_id, request_open, chunks FROM exchanges WHERE request_open IS NOT NULL').all())
      for (const [index, row] of opens.entries()) {
        const prefix = recoverCompleted ? `recovery-${randomUUID()}-replay` : 'replay'
        const input = join(directory, `${prefix}-${index}.json`); const output = join(directory, `${prefix}-${index}.chunks.json`)
        writeFileSync(input, row.request_open, { flag: 'wx', mode: 0o600 })
        execFileSync(probe, ['replay', join(devicePath.deviceData, 'providers'), input, output], { encoding: 'utf8' })
        assert.equal(readFileSync(output, 'utf8'), row.chunks)
      }
      assert.deepEqual(readDeviceExecutionReceipts(directory).calls, retainedCalls)
      return { bindingsPreserved: true, messagesPreserved: true, exactReplayCalls: opens.length }
    },
  } })
  retain('restored.json', restored)
  const secrets = [authA, authB].flatMap(auth => { const tokens = JSON.parse(readFileSync(join(auth, 'tokens.json'))); return [tokens.access_token, tokens.refresh_token] })
  for (const file of globSync('**/*', { cwd: directory })) {
    if (file.startsWith('device-data/')) continue // Device-private storage has a different boundary.
    if (!statSync(join(directory, file)).isFile()) continue
    const bytes = readFileSync(join(directory, file))
    assert.ok(secrets.every(secret => !bytes.includes(Buffer.from(secret))), `secret leaked to ${file}`)
  }
  retain('complete.json', { complete: true, calls: retainedCalls.length, evidenceSha256: createHash('sha256').update(JSON.stringify(retainedCalls)).digest('hex'),
    billing: 'Go enabled and Use balance disabled confirmed by user; no billing-control API verification' })
  console.log(JSON.stringify({ complete: true, calls: retainedCalls.length, directory }))
}
try { await run() } catch (error) {
  // Error messages contain only local assertions and already-redacted runtime diagnostics.
  retain(resumeBeforeCalls || recoverCompleted ? `failure-recovery-${randomUUID()}.json` : 'failure.json', { complete: false, message: error instanceof Error ? error.message : 'live acceptance failed' })
  console.error('OpenCode acceptance failed; retained evidence must be inspected before any new call')
  process.exitCode = 1
}
