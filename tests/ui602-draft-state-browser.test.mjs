import assert from 'node:assert/strict'
import { existsSync, mkdtempSync, rmSync } from 'node:fs'
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
const fixturePath = 'tests/fixtures/browser-settings-draft-state-client.mjs'

test('real Chrome keeps Settings drafts separate from live server snapshots', async t => {
  const chromePath = chromeBinary()
  if (chromePath === null) {
    t.skip('Chrome is not installed')
    return
  }
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-settings-draft-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath,
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  const launched = await DevTools.launch({
    chromePath,
    directory,
    debugPort: await freePort(),
  })
  const { chrome, devtools } = launched
  const exceptions = []
  let sessionId = null
  t.after(async () => {
    devtools.close()
    await Promise.all([stopChild(chrome, 'SIGTERM'), closeServer(clientServer)])
    rmSync(directory, { recursive: true, force: true })
    assert.equal(existsSync(directory), false)
  })
  devtools.on('Runtime.exceptionThrown', event => { exceptions.push(event) })
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  ;({ sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true }))
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runSettingsDraftStateScenario')

  const result = await evaluate(devtools, sessionId, 'runSettingsDraftStateScenario()')
  assert.deepEqual(result.conflict, {
    cleanConcurrency: '3',
    cleanModel: 'server-model-b',
    dirtyProvider: 'browser-provider',
    focused: true,
    message: 'The server changed this draft. Provider ID: server “server-provider-b”; your draft “browser-provider”.',
    visible: true,
  })
  assert.deepEqual(result.submitted, ['updateSettings', {
    defaultModelRoute: {
      credentialReferenceId: 'crd_00000000000000000000000001',
      modelId: 'server-model-b',
      providerId: 'browser-provider',
    },
    workerConcurrencyLimit: 3,
  }])
  assert.deepEqual(result.failure, { retained: 'browser-provider' })
  assert.deepEqual(result.discarded, {
    concurrency: '4',
    model: 'server-model-c',
    provider: 'server-provider-c',
  })
  assert.equal(result.secret.afterFailure, 'BROWSER_ONLY_SECRET')
  assert.equal(result.secret.afterCancel, '')
  assert.deepEqual(result.secret.submitted, ['createCredentialReference', {
    credentialReferenceId: 'crd_00000000000000000000000002',
    displayName: 'Browser new Credential',
    providerId: 'server-provider-c',
    vaultLocator: 'BROWSER_ONLY_SECRET',
  }])
  assert.equal(result.secret.localStorage.includes('BROWSER_ONLY_SECRET'), false)
  assert.equal(result.secret.sessionStorage.includes('BROWSER_ONLY_SECRET'), false)
  assert.equal(exceptions.length, 0, JSON.stringify(exceptions))
})
