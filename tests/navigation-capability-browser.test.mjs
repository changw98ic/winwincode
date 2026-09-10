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
} from './fixtures/real-browser-harness.mjs'

const root = resolve(import.meta.dirname, '..')

async function waitForMode(devtools, sessionId, mode) {
  const deadline = Date.now() + 20_000
  while (Date.now() < deadline) {
    try {
      if (await evaluate(devtools, sessionId, `globalThis.navigationMode === ${JSON.stringify(mode)}`)) {
        return
      }
    } catch {}
    await new Promise(resolvePromise => setTimeout(resolvePromise, 50))
  }
  throw new Error(`navigation browser fixture did not load ${mode}`)
}

test('real Chrome projects personal, enterprise, disabled, and read-only navigation', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for navigation validation')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-navigation-capability-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-navigation-capability.mjs',
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

  async function navigate(mode) {
    await devtools.send('Page.navigate', {
      url: `https://client.localhost:${String(clientPort)}/?mode=${mode}#/chat`,
    }, sessionId)
    await waitForMode(devtools, sessionId, mode)
    return evaluate(devtools, sessionId, 'globalThis.inspectNavigationCapability()')
  }

  const personal = await navigate('personal')
  assert.equal(personal.deployment, 'personal')
  assert.deepEqual(Object.keys(personal.entries).sort(), [
    'chat', 'extensions', 'home', 'projects', 'settings',
  ])
  const directDenial = await evaluate(
    devtools,
    sessionId,
    'globalThis.openDeniedEnterpriseRoute()',
  )
  assert.equal(directDenial.alertRole, 'alert')
  assert.equal(directDenial.enterpriseQueries, 0)
  assert.equal(directDenial.focused, true)
  assert.equal(directDenial.safeHref, '#/chat')
  assert.match(directDenial.text, /无法使用.*返回新对话/u)

  const enterprise = await navigate('enterprise')
  assert.equal(enterprise.deployment, 'enterprise')
  assert.deepEqual(Object.keys(enterprise.entries).sort(), [
    'chat', 'extensions', 'home', 'projects', 'settings',
  ])
  assert.equal(enterprise.entries.enterprise, undefined)
  const websocketRevoked = await evaluate(
    devtools,
    sessionId,
    'globalThis.revokeEnterpriseSubscription()',
  )
  assert.equal(websocketRevoked.subscriptionClosed, true)
  assert.equal(websocketRevoked.safeHref, '#/chat')
  assert.equal(websocketRevoked.subscriptionClosed, true)

  await navigate('enterprise')
  const revoked = await evaluate(devtools, sessionId, 'globalThis.revokeEnterpriseRoute()')
  assert.equal(revoked.subscriptionClosed, true)
  assert.equal(revoked.visibleEntries, 0)
  assert.match(revoked.routeText, /Sign in/iu)

  // Design shell: denied/read-only enterprise areas render no navigation
  // entry; capability facts are covered by the unit lane.
  const disabled = await navigate('disabled')
  assert.equal(disabled.entries.enterprise, undefined)
  assert.equal(disabled.deployment, 'enterprise')

  const readOnly = await navigate('read-only')
  assert.equal(readOnly.entries.enterprise, undefined)
  assert.equal(readOnly.deployment, 'enterprise')
})
