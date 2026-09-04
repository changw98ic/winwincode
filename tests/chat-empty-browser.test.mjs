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

test('a real browser can create the first Chat from an empty repository without reloading', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the empty Chat browser test')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-empty-chat-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-chat-empty-client.mjs',
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
    url: `https://client.localhost:${String(clientPort)}/#/chat`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runEmptyChatScenario')
  const result = await evaluate(devtools, sessionId, 'globalThis.runEmptyChatScenario()')

  assert.equal(result.empty.hash, '#/chat')
  assert.equal(result.empty.newChatDisabled, false)
  assert.match(result.empty.text, /first Chat/iu)
  assert.match(result.empty.model, /Repository scope/iu)
  assert.match(result.empty.model, /Browser Provider.*Browser Model/iu)
  assert.match(result.notConfigured.text, /No Provider/iu)
  assert.equal(result.notConfigured.settingsHref, '#/settings')
  assert.equal(result.denied.newChatDisabled, true)
  assert.match(result.denied.text, /do not have access/iu)
  assert.doesNotMatch(result.denied.text, /private browser/iu)
  assert.equal(result.revoked.modelDisabled, true)
  assert.equal(result.revoked.newChatDisabled, true)
  assert.match(result.revoked.model, /Credential missing or revoked/iu)
  assert.equal(result.crossProject.newChatDisabled, true)
  assert.match(result.crossProject.text, /could not be updated/iu)
  assert.match(result.created.hash, /^#\/chat\?session=psn_[0-9A-HJKMNP-TV-Z]{26}$/u)
  assert.equal(result.created.heading, 'New Chat')
  assert.equal(result.created.status, 'Ready')
  assert.equal(result.created.sendDisabled, false)
  assert.equal(result.calls.commands.length, 1)
  assert.equal(
    result.calls.queries.some(call => call.query === 'model.route.availability.list'),
    true,
  )
  assert.deepEqual(result.calls.commands[0], {
    schemaVersion: 'winwincode/v1',
    requestId: result.calls.commands[0].requestId,
    actor: { kind: 'user', id: 'usr_00000000000000000000000001' },
    scope: {
      kind: 'repository',
      organizationId: 'org_00000000000000000000000001',
      workspaceId: 'wsp_00000000000000000000000001',
      projectId: 'prj_00000000000000000000000001',
      repositoryId: 'rep_00000000000000000000000001',
    },
    command: 'session.create',
    expectedRevision: 0,
    payload: {
      productSessionId: result.calls.commands[0].payload.productSessionId,
      projectId: 'prj_00000000000000000000000001',
      repositoryId: 'rep_00000000000000000000000001',
      title: 'New Chat',
      modelRoute: {
        providerId: 'browser-provider-two',
        modelId: 'browser-model-two',
        credentialReferenceId: 'crd_00000000000000000000000002',
      },
    },
  })
  const sessionSubscription = result.calls.subscriptions.find(
    call => call.subscription.stream.kind === 'product-session',
  )
  assert.deepEqual(sessionSubscription.subscription.stream, {
    kind: 'product-session',
    productSessionId: result.calls.commands[0].payload.productSessionId,
  })
  const scopeSubscriptions = result.calls.subscriptions.filter(
    call => call.subscription.stream.kind === 'scope',
  )
  assert.equal(scopeSubscriptions.some(call => (
    call.subscription.scope.kind === 'project'
    && call.subscription.scope.projectId === result.calls.commands[0].payload.projectId
    && !('repositoryId' in call.subscription.scope)
  )), true)
})
