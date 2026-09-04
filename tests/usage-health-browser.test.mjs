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

test('a real browser renders the Usage, Provider and Worker health summary on the diagnostics page', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for usage health validation')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-usage-health-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-usage-health.mjs',
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
    url: `https://client.localhost:${String(clientPort)}/#/settings`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'usageHealthReady')

  const summary = await evaluate(devtools, sessionId, 'globalThis.openDiagnosticsUsageHealth()')
  assert.equal(summary.present, true)
  assert.match(summary.heading, /Usage, Provider and Worker health/u)
  assert.match(summary.updated, /Last updated \d{4}-\d{2}-\d{2}T/u)
  assert.match(summary.window, /\d{4}-\d{2}-\d{2}T/u)
  assert.match(summary.window, /1 of 1 sessions/u)
  assert.equal(
    summary.liveRegions,
    0,
    'the summary must not open a second polite channel next to the page one',
  )

  assert.equal(summary.deliveries.length, 1)
  assert.match(summary.deliveries[0].usage, /input_tokens 120/u)
  assert.match(summary.deliveries[0].usage, /total_tokens 180/u)

  assert.equal(summary.stageRuns.length, 2)
  assert.deepEqual(
    summary.stageRuns.map(row => row.unknown).sort(),
    ['false', 'true'],
    'the StageRun without usage must be the only unknown-marked row',
  )

  assert.deepEqual(summary.providers.map(row => row.state).sort(), ['ready', 'unavailable'])
  assert.equal(summary.models, 2)
  assert.deepEqual(summary.unknownMarkers.every(value => value === 'true'), true)

  assert.deepEqual(summary.workers.map(row => row.state), [
    'online',
    'draining',
    'offline',
  ])
  assert.deepEqual(new Set(summary.workers.map(row => row.tone)).size, 3)
  assert.match(summary.workers[0].label, /Online, accepting work/u)
  assert.match(summary.workers[1].label, /Draining/u)
  assert.match(summary.workers[2].label, /Offline/u)

  assert.match(summary.capacityState, /sufficient/u)
  assert.match(summary.credentials.join(' '), /Credential available · rotation 3/u)

  assert.equal(summary.errors.length, 2)
  assert.match(summary.errors.join(' '), /2 failures/u)
  assert.match(summary.errors.join(' '), /recovery in progress or complete/u)
  assert.match(summary.errors.join(' '), /1 open attention items/u)

  assert.equal(summary.leak, false, 'no secret material or credential id may reach the summary')

  const refreshed = await evaluate(devtools, sessionId, 'globalThis.refreshUsageHealth()')
  assert.equal(refreshed.present, true)
  assert.deepEqual(refreshed.deliveries.map(row => row.key), summary.deliveries.map(row => row.key))
  assert.equal(refreshed.leak, false)

  // A projection this fixture never implements must mark only its own section.
  const degraded = await evaluate(devtools, sessionId, 'globalThis.blockProviderRead()')
  assert.equal(degraded.providers, 0)
  assert.equal(degraded.models, 0)
  assert.equal(degraded.providerSectionUnavailable, true)
  assert.equal(degraded.visibleUnavailableNotes, 1)
  assert.equal(degraded.deliveryRowsStillPresent, true)
  assert.equal(degraded.leak, false)
})
