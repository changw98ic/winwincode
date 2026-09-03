// SPDX-License-Identifier: Apache-2.0

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

async function launch(t, fixturePath) {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the Diff viewer test')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-candidate-diff-'))
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
  t.after(async () => {
    launched.devtools?.close()
    await Promise.all([
      stopChild(launched.chrome, 'SIGTERM'),
      closeServer(clientServer),
    ])
    rmSync(directory, { recursive: true, force: true })
  })
  const { targetId } = await launched.devtools.send('Target.createTarget', { url: 'about:blank' })
  const { sessionId } = await launched.devtools.send('Target.attachToTarget', {
    targetId,
    flatten: true,
  })
  await launched.devtools.send('Runtime.enable', {}, sessionId)
  await launched.devtools.send('Page.enable', {}, sessionId)
  return { devtools: launched.devtools, sessionId, clientPort }
}

const route = 'https://client.localhost:PORT/#/strongflow'
  + '?delivery=dlv_00000000000000000000000001'
  + '&session=psn_00000000000000000000000001'
  + '&stageRun=run_00000000000000000000000001'
  + '&file=src%2Frenamed.ts&view=unified'

test('real Chrome reviews the Candidate Diff in both layouts with keyboard and search', async t => {
  const { devtools, sessionId, clientPort } = await launch(
    t,
    'tests/fixtures/browser-strongflow-candidate-diff.mjs',
  )
  await devtools.send('Page.navigate', { url: route.replace('PORT', String(clientPort)) }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runCandidateDiffScenario')
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 1280,
    height: 900,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  const result = await evaluate(devtools, sessionId, 'globalThis.runCandidateDiffScenario()')

  assert.equal(result.initial.columns, '3')
  assert.equal(result.initial.hunkHeaders.length, 2)
  assert.match(result.initial.hunkHeaders[0], /@@ -10,7 \+10,7 @@ function before\(\) \{/u)
  assert.match(result.initial.deletionLine, /-const beta = 2$/u)
  assert.match(result.initial.additionLine, /\+const beta = 22$/u)
  assert.deepEqual(result.initial.pressed, ['unified:true', 'side-by-side:false'])
  assert.match(result.initial.status, /first 320 of 640 Diff bytes/u)
  assert.match(result.initial.fileSummary, /3 files loaded/u)
  assert.equal(result.initial.selectedPath, 'src/renamed.ts')
  assert.match(result.initial.hash, /&file=src%2Frenamed\.ts&view=unified$/u)
  assert.equal(result.mainCount, 1)

  assert.match(result.search.matchStatus, /Match 1 of 1/u)
  assert.match(result.search.activeText, /\+const kappa = 3/u)

  assert.equal(result.collapsed.focusedHunk, 'hunk:1')
  assert.match(result.collapsed.hiddenNote, /6 unchanged lines hidden/u)
  assert.equal(result.collapsed.contextRowsInFirstHunk, 0)
  assert.equal(result.collapsed.contextToggle, 'Show unchanged lines')

  assert.equal(result.switched.columns, '4')
  assert.match(result.switched.modifiedLine, /-const beta = 2/u)
  assert.match(result.switched.modifiedLine, /\+const beta = 22/u)
  assert.equal(result.switched.searchDraft, 'kappa', 'the search draft survives a layout change')
  assert.equal(result.switched.selectedPath, 'src/renamed.ts', 'the file selection is unchanged')
  assert.match(result.switched.hash, /&file=src%2Frenamed\.ts&view=side-by-side$/u)
  assert.deepEqual(result.switched.pressed, ['unified:false', 'side-by-side:true'])
  assert.deepEqual(result.switched.calls, [['viewMode', 'side-by-side']])

  assert.equal(result.backToUnified.columns, '3')

  assert.match(result.loadedMore.status, /6 of 12 Diff lines shown\.$/u)
  assert.equal(result.loadedMore.loadMoreHidden, true)

  assert.match(result.binary.status, /Binary file preview is unavailable\./u)
  assert.equal(result.binary.rowCount, 0)
  assert.match(result.binary.hash, /&file=public%2Flogo\.png&view=unified$/u)
})

test('real Chrome falls back to the unified layout on narrow viewports', async t => {
  const { devtools, sessionId, clientPort } = await launch(
    t,
    'tests/fixtures/browser-strongflow-candidate-diff.mjs',
  )
  await devtools.send('Page.navigate', { url: route.replace('PORT', String(clientPort)) }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runCandidateDiffScenario')
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 560,
    height: 900,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  const result = await evaluate(devtools, sessionId, 'globalThis.runCandidateDiffNarrowScenario()')

  assert.equal(result.columns, '3', 'a narrow viewport renders the unified layout')
  assert.equal(result.disabled, true, 'the side-by-side option is disabled while narrow')
  assert.equal(result.narrow, 'true')
  assert.match(result.hash, /&view=unified$/u, 'the route keeps the canonical unified value')
  await devtools.send('Emulation.clearDeviceMetricsOverride', {}, sessionId)
})
