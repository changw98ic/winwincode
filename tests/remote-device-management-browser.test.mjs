// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'
import { chromeBinary, closeServer, command, DevTools, evaluate, freePort,
  listen, staticClientServer, stopChild, waitForGlobal } from './fixtures/real-browser-harness.mjs'

const root = resolve(import.meta.dirname, '..')

test('Device route pairs and permanently revokes a remote Device and exposes application recovery', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'wwc-remote-device-'))
  const server = staticClientServer({ root, fixturePath: 'tests/fixtures/browser-remote-device-management.mjs', configuration: () => ({}) })
  const origin = `http://127.0.0.1:${String(await listen(server))}`
  let chrome = null
  let devtools = null
  t.after(async () => {
    devtools?.close()
    await Promise.all([...(chrome === null ? [] : [stopChild(chrome, 'SIGTERM')]), closeServer(server)])
    rmSync(directory, { recursive: true, force: true })
  })
  const launched = await DevTools.launch({ chromePath, directory, debugPort: await freePort() })
  chrome = launched.chrome; devtools = launched.devtools
  const exceptions = []
  devtools.on('Runtime.exceptionThrown', event => {
    exceptions.push(event.exceptionDetails?.exception?.description ?? event.exceptionDetails?.text)
  })
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  const { sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true })
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 1280, height: 900, deviceScaleFactor: 1, mobile: false,
  }, sessionId)
  await devtools.send('Page.navigate', { url: origin }, sessionId)
  try {
    await waitForGlobal(devtools, sessionId, 'remoteDeviceReady')
  } catch (error) {
    const visible = await evaluate(devtools, sessionId, 'document.body.innerText')
    throw new Error(`${error.message}\n${exceptions.join('\n')}\n${visible}`)
  }
  const paired = await evaluate(devtools, sessionId, 'globalThis.pairRemoteDeviceFixture()')
  assert.equal(paired.hash, '#/device')
  assert.equal(paired.visible, true)
  assert.equal(paired.calls, 1)
  assert.match(paired.command, /WWC_DEVICE_PAIRING_TOKEN=.*wwc device bind.*--server-url 'https:\/\/server.fixture'.*--ca-file \/ABS\/ca.pem/u)
  assert.match(paired.expiry, /2099/u)
  async function screenshot(name) {
    const evidence = process.env.WWC_UI_EVIDENCE_DIRECTORY
    if (evidence === undefined) return
    mkdirSync(evidence, { recursive: true })
    const image = await devtools.send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true }, sessionId)
    writeFileSync(join(evidence, name), Buffer.from(image.data, 'base64'))
  }
  await screenshot('device-pairing-fixture.png')
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 390, height: 844, deviceScaleFactor: 1, mobile: true,
  }, sessionId)
  assert.equal(await evaluate(devtools, sessionId,
    'document.documentElement.scrollWidth <= innerWidth + 1'), true)
  await screenshot('device-pairing-mobile-fixture.png')
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 1280, height: 900, deviceScaleFactor: 1, mobile: false,
  }, sessionId)
  const revoked = await evaluate(devtools, sessionId, 'globalThis.revokeRemoteDeviceFixture()')
  assert.equal(revoked.beforeConfirmation, 0)
  assert.equal(revoked.confirmationVisible, true)
  assert.deepEqual(revoked.calls, ['cnd_00000000000000000000000001'])
  assert.match(revoked.status, /已永久吊销/u)
  const blocked = await evaluate(devtools, sessionId, 'globalThis.showCandidateApplicationFixture("blocked")')
  assert.equal(blocked.disabled, true)
  assert.match(blocked.status, /结果尚未确认.*暂停再次应用/su)
  await screenshot('candidate-blocked-fixture.png')
  const retry = await evaluate(devtools, sessionId, 'globalThis.showCandidateApplicationFixture("awaiting_retry")')
  assert.equal(retry.disabled, false)
  assert.equal(retry.branch, 'release/original')
  assert.equal(retry.head, 'a'.repeat(40))
  const applied = await evaluate(devtools, sessionId, 'globalThis.submitCandidateApplicationFixture()')
  assert.equal(applied.applications.length, 1)
  assert.equal(applied.applications[0].targetBranch, 'release/original')
  assert.equal(applied.applications[0].expectedHead, 'a'.repeat(40))
  assert.equal(applied.disabled, true)
  assert.deepEqual(exceptions, [])
})
