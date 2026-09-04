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
const evidenceId = 'evd_00000000000000000000000001'

test('a real browser opens Evidence, Tests, and Logs tabs with exact bindings and deep links', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(
    chromePath,
    null,
    'Chrome or Chromium is required for the Evidence workbench test',
  )
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-evidence-strongflow-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-strongflow-evidence-client.mjs',
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
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 1280,
    height: 900,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}/#/strongflow`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runEvidenceWorkbenchScenario')
  const result = await evaluate(devtools, sessionId, 'globalThis.runEvidenceWorkbenchScenario()')

  assert.match(result.initial.hash, /^#\/strongflow\?delivery=dlv_00000000000000000000000002/u)
  assert.deepEqual(result.initial.tabLabels, ['Evidence', 'Tests', 'Logs'])
  assert.equal(result.initial.selected, 'Evidence')
  assert.deepEqual(result.initial.rowTypes, ['test', 'command', 'runtime_event', 'diff'])
  assert.deepEqual(result.initial.candidateStates, ['current'])
  assert.deepEqual(result.initial.panels.map(panel => panel.role), ['tabpanel', 'tabpanel', 'tabpanel'])
  assert.equal(result.initial.panels.filter(panel => !panel.hidden).length, 1)
  assert.deepEqual(result.initial.entryPoints, { stage: 4, candidate: 4, criterion: 1 })
  assert.match(result.initial.summary, /1 criteria · 0 passed · 1 failed/u)
  assert.match(result.initial.criterionJoin, /criterion:browser/u)

  assert.deepEqual(result.testsView.rowTypes, ['test'])
  assert.match(result.testsView.hash, /tab=tests/u)
  assert.match(result.testsView.hash, /delivery=dlv_00000000000000000000000002/u)

  assert.equal(result.detail.outcome, 'Failed · business')
  assert.equal(result.detail.tone, 'business-fail')
  assert.equal(result.detail.statusIcon, '×')
  assert.equal(result.detail.statusIconHidden, 'true')
  assert.equal(result.detail.candidate, 'current candidate')
  assert.match(result.detail.artifact, /download-only/u)
  assert.match(result.detail.hash, /evidence=evd_00000000000000000000000001/u)
  assert.match(result.detail.hash, /tab=tests/u)
  assert.equal(result.detail.stableNode, true)
  assert.equal(result.detail.closeFocusedDuringLoad, true)
  assert.equal(result.detail.busy, 'false')
  assert.equal(result.detail.artifactSelectors, 2)
  assert.equal(result.detail.selectedArtifact, 'true')

  assert.equal(result.closed.hash.includes('evidence='), false)
  assert.equal(result.closed.detailRetained, true)
  assert.equal(result.closed.detailHidden, true)
  assert.equal(result.closed.openerFocused, true)
  assert.equal(result.refreshed.staleClearedDuringRefresh, true)
  assert.equal(result.refreshed.stableNode, true)
  assert.deepEqual(result.refreshed.stableEntryPoints, {
    stage: true,
    candidate: true,
    criterion: true,
  })
  assert.match(result.refreshed.binding, /revision 3/u)
  assert.equal(result.navigationEntryCount, 1)

  assert.deepEqual(result.evidenceQueries, [
    {
      evidenceId,
      readPageLimit: 1,
      cursorToken: 'cursor_00000000000000000000000000000002',
      page: { cursor: null, limit: 1 },
    },
    {
      evidenceId,
      readPageLimit: 1,
      cursorToken: 'cursor_00000000000000000000000000000003',
      page: { cursor: null, limit: 1 },
    },
  ])
  assert.equal(result.contentQueries, 0)

  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}/${result.detail.hash}`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runEvidenceDeepLinkReloadScenario')
  const reloaded = await evaluate(
    devtools,
    sessionId,
    'globalThis.runEvidenceDeepLinkReloadScenario()',
  )
  assert.equal(reloaded.hash, result.detail.hash)
  assert.equal(reloaded.selectedTab, 'Tests')
  assert.equal(reloaded.detailEvidenceId, evidenceId)
  assert.deepEqual(reloaded.route, {
    deliveryId: 'dlv_00000000000000000000000002',
    productSessionId: 'psn_00000000000000000000000002',
    stageRunId: 'run_00000000000000000000000001',
    evidenceId,
  })
  assert.deepEqual(reloaded.binding, {
    deliveryId: 'dlv_00000000000000000000000002',
    sessionBindingId: 'binding:strongflow:evidence-browser',
    stageRunId: 'run_00000000000000000000000001',
    evidenceId,
  })
  assert.equal(reloaded.evidenceQueryCount >= 1, true)

  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 700,
    height: 900,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  const compactDisplay = await evaluate(
    devtools,
    sessionId,
    "getComputedStyle(document.querySelector('.wwc-strongflow-evidence-row')).display",
  )
  assert.equal(compactDisplay, 'grid')
})
