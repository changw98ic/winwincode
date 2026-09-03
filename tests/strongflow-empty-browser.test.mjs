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

test('a real browser creates the first Delivery and opens subscribed StrongFlow without reloading', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the empty StrongFlow test')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-empty-strongflow-'))
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
    url: `https://client.localhost:${String(clientPort)}/#/strongflow`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runEmptyStrongFlowScenario')
  const result = await evaluate(devtools, sessionId, 'globalThis.runEmptyStrongFlowScenario()')

  assert.equal(result.empty.hash, '#/strongflow')
  assert.equal(result.empty.submitDisabled, false)
  assert.match(result.empty.text, /Create the first Delivery/iu)
  assert.match(
    result.created.hash,
    /^#\/strongflow\?delivery=dlv_[0-9A-HJKMNP-TV-Z]{26}&session=psn_[0-9A-HJKMNP-TV-Z]{26}&stageRun=run_[0-9A-HJKMNP-TV-Z]{26}&view=unified$/u,
  )
  assert.equal(result.created.heading, 'First StrongFlow Delivery')
  assert.match(result.created.status, /clarifying.*revision 2/iu)
  assert.deepEqual(result.created.listDeliveryIds, [result.deliveryId])
  assert.deepEqual(result.calls.commands.map(commandCall => ({
    ...commandCall,
    requestId: 'REQUEST_ID',
    payload: {
      ...commandCall.payload,
      deliveryId: 'DELIVERY_ID',
    },
  })), [{
    schemaVersion: 'winwincode/v1',
    requestId: 'REQUEST_ID',
    actor: { kind: 'user', id: 'usr_00000000000000000000000001' },
    scope: result.scope,
    command: 'delivery.create',
    expectedRevision: 0,
    payload: {
      deliveryId: 'DELIVERY_ID',
      spec: {
        acceptanceCriteria: [{
          id: 'criterion:1',
          required: true,
          title: 'The real Delivery snapshot opens.',
        }, {
          id: 'criterion:2',
          required: true,
          title: 'Delivery events are subscribed.',
        }],
        baseRevision: '0123456789abcdef0123456789abcdef01234567',
        constraints: [],
        goal: 'Enter StrongFlow from an empty repository.',
        outOfScope: [],
        publicationTarget: null,
        repositoryId: result.scope.repositoryId,
        scope: ['Enter StrongFlow from an empty repository.'],
        sourceProductSessionId: null,
        title: 'First StrongFlow Delivery',
      },
      tasks: [],
    },
  }, {
    schemaVersion: 'winwincode/v1',
    requestId: 'REQUEST_ID',
    actor: { kind: 'user', id: 'usr_00000000000000000000000001' },
    scope: result.scope,
    command: 'delivery.advance',
    expectedRevision: 1,
    payload: { deliveryId: 'DELIVERY_ID' },
  }])
  assert.deepEqual(result.calls.subscriptions[0].subscription.stream, {
    kind: 'delivery',
    deliveryId: result.deliveryId,
  })
  assert.equal(result.navigationEntryCount, 1)
})
