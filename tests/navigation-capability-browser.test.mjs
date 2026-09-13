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

test('real Chrome projects personal, disabled, and read-only navigation without Enterprise', async t => {
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
    'attention', 'chat', 'home', 'settings', 'strongflow',
  ])
  assert.equal(personal.entries.enterprise, undefined)

  const disabled = await navigate('disabled')
  assert.equal(disabled.entries.chat.capability, 'disabled')
  assert.equal(disabled.entries.chat.ariaDisabled, 'true')
  assert.equal(disabled.entries.chat.tabIndex, -1)
  assert.match(disabled.entries.chat.label, /unavailable/iu)
  const blocked = await evaluate(devtools, sessionId, 'globalThis.tryDisabledChatEntry()')
  assert.equal(blocked.after, blocked.before)

  const readOnly = await navigate('read-only')
  assert.equal(readOnly.entries.chat.capability, 'read-only')
  assert.equal(readOnly.entries.chat.ariaDisabled, null)
  assert.match(readOnly.entries.chat.label, /read only/iu)
})
