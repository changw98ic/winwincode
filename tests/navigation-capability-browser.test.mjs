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
    'approvals', 'chat', 'settings', 'strongflow',
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
  assert.match(directDenial.text, /not available.*Return to Chat/iu)

  const enterprise = await navigate('enterprise')
  assert.equal(enterprise.deployment, 'enterprise')
  assert.deepEqual(Object.keys(enterprise.entries).sort(), [
    'approvals', 'chat', 'enterprise', 'settings', 'strongflow',
  ])
  assert.equal(enterprise.entries.enterprise.capability, 'available')
  const revoked = await evaluate(devtools, sessionId, 'globalThis.revokeEnterpriseRoute()')
  assert.equal(revoked.subscriptionClosed, true)
  assert.equal(revoked.visibleEntries, 0)
  assert.match(revoked.routeText, /Sign in/iu)

  const disabled = await navigate('disabled')
  assert.equal(disabled.entries.enterprise.capability, 'disabled')
  assert.equal(disabled.entries.enterprise.ariaDisabled, 'true')
  assert.equal(disabled.entries.enterprise.tabIndex, -1)
  assert.match(disabled.entries.enterprise.label, /unavailable/iu)
  const blocked = await evaluate(devtools, sessionId, 'globalThis.tryDisabledEnterpriseEntry()')
  assert.equal(blocked.after, blocked.before)

  const readOnly = await navigate('read-only')
  assert.equal(readOnly.entries.enterprise.capability, 'read-only')
  assert.equal(readOnly.entries.enterprise.ariaDisabled, null)
  assert.match(readOnly.entries.enterprise.label, /read only/iu)
})
