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

test('real Chrome projects personal, organization, disabled, and read-only navigation', async t => {
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
  // An organization-hierarchy Scope projects the enterprise deployment fact;
  // the community client still renders no management surface for it.
  const organization = await navigate('enterprise')
  assert.equal(organization.deployment, 'enterprise')
  assert.deepEqual(Object.keys(organization.entries).sort(), [
    'chat', 'extensions', 'home', 'projects', 'settings',
  ])
  assert.equal(organization.entries.enterprise, undefined)

  // Design shell: denied/read-only entries render their capability instead of
  // pretending the area is usable; the unit lane covers the reason mapping.
  const disabled = await navigate('disabled')
  assert.equal(disabled.deployment, 'enterprise')
  assert.equal(disabled.entries.home.capability, 'disabled')
  assert.equal(disabled.entries.home.ariaDisabled, 'true')

  const readOnly = await navigate('read-only')
  assert.equal(readOnly.deployment, 'enterprise')
  assert.equal(readOnly.entries.home.capability, 'read-only')
  assert.match(readOnly.entries.home.label, /只读/u)
})
