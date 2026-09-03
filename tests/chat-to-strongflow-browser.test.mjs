import assert from 'node:assert/strict'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  certificate,
  chromeBinary,
  closeServer,
  command,
  DevTools,
  evaluate,
  freePort,
  listen,
  staticClientServer,
  stopChild,
  waitForGlobal,
} from './fixtures/real-browser-harness.mjs'

const root = resolve(import.meta.dirname, '..')
const productSessionId = 'psn_00000000000000000000000001'

test('a real browser confirms Chat requirements then opens and restores exact StrongFlow', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for Chat conversion')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-chat-strongflow-convert-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-strongflow-empty-client.mjs',
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  let chrome = null
  let devtools = null
  t.after(async () => {
    devtools?.close()
    await Promise.all([
      ...(chrome === null ? [] : [stopChild(chrome, 'SIGTERM')]),
      closeServer(clientServer),
    ])
    rmSync(directory, { recursive: true, force: true })
  })

  const launched = await DevTools.launch({
    chromePath,
    directory,
    debugPort: await freePort(),
  })
  chrome = launched.chrome
  devtools = launched.devtools
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  const { sessionId } = await devtools.send('Target.attachToTarget', {
    targetId,
    flatten: true,
  })
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}/#/chat?session=${productSessionId}`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runChatConversionScenario')
  const result = await evaluate(devtools, sessionId, 'globalThis.runChatConversionScenario()')

  assert.equal(result.draft.sourceSession, productSessionId)
  assert.equal(result.draft.title, 'Confirmed requirements Chat')
  assert.equal(result.draft.goal, 'Build the requirement confirmed by this Chat.')
  assert.match(result.draft.scope, /rep_00000000000000000000000001/u)
  assert.equal(result.draft.model, 'browser-provider / browser-model')
  assert.match(
    result.created.hash,
    /^#\/strongflow\?delivery=dlv_[0-9A-HJKMNP-TV-Z]{26}&session=psn_[0-9A-HJKMNP-TV-Z]{26}&stageRun=run_[0-9A-HJKMNP-TV-Z]{26}$/u,
  )
  assert.equal(result.created.heading, 'Confirmed requirements Chat')
  assert.match(result.created.status, /Waiting for your input/u)
  assert.deepEqual(result.created.listDeliveryIds, [result.deliveryId])
  assert.deepEqual(result.calls.commands.map(request => ({
    ...request,
    requestId: 'REQUEST_ID',
    payload: { ...request.payload, deliveryId: 'DELIVERY_ID' },
  })), [{
    schemaVersion: 'winwincode/v1',
    requestId: 'REQUEST_ID',
    actor: result.actor,
    scope: result.scope,
    command: 'delivery.create',
    expectedRevision: 0,
    payload: {
      deliveryId: 'DELIVERY_ID',
      spec: {
        acceptanceCriteria: [{
          id: 'criterion:1',
          required: true,
          title: 'The confirmed requirement is delivered.',
        }],
        baseRevision: '0123456789abcdef0123456789abcdef01234567',
        goal: 'Build the requirement confirmed by this Chat.',
        publicationTarget: null,
        repositoryId: result.scope.repositoryId,
        title: 'Confirmed requirements Chat',
      },
      tasks: [],
    },
  }, {
    schemaVersion: 'winwincode/v1',
    requestId: 'REQUEST_ID',
    actor: result.actor,
    scope: result.scope,
    command: 'delivery.advance',
    expectedRevision: 1,
    payload: { deliveryId: 'DELIVERY_ID' },
  }])
  assert.equal(result.calls.subscriptions.some(call => (
    call.subscription.stream.kind === 'delivery'
    && call.subscription.stream.deliveryId === result.deliveryId
  )), true)

  await devtools.send('Page.reload', {}, sessionId)
  await waitForGlobal(devtools, sessionId, 'inspectConvertedAfterReload')
  const restored = await evaluate(devtools, sessionId, 'globalThis.inspectConvertedAfterReload()')
  assert.equal(restored.hash, result.created.hash)
  assert.equal(restored.heading, 'Confirmed requirements Chat')
  assert.match(restored.status, /Waiting for your input/u)
  assert.equal(restored.deliverySubscribed, true)
})
