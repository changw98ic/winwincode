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
  + `&candidate=${encodeURIComponent(`git-candidate:sha256:${'1'.repeat(64)}`)}`
  + '&panel=candidate&file=src%2Frenamed.ts&view=unified&line=11'
  + '&organizationId=org_00000000000000000000000001'
  + '&workspaceId=wsp_00000000000000000000000001'
  + '&projectId=prj_00000000000000000000000001'
  + '&repositoryId=rep_00000000000000000000000001'
  + '&task=task%3Ahistory'
  + '&run=run_00000000000000000000000004'

const routeContext = {
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
  task: 'task:history',
  run: 'run_00000000000000000000000004',
}

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
  const beforeReload = await evaluate(devtools, sessionId, 'globalThis.candidateDeepLinkSnapshot()')
  await devtools.send('Page.reload', { ignoreCache: true }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runCandidateDiffScenario')
  const afterReload = await evaluate(devtools, sessionId, 'globalThis.candidateDeepLinkSnapshot()')
  assert.deepEqual(afterReload, beforeReload)
  assert.deepEqual(afterReload, {
    route: {
      file: 'src/renamed.ts',
      view: 'unified',
      organizationId: routeContext.organizationId,
      workspaceId: routeContext.workspaceId,
      projectId: routeContext.projectId,
      repositoryId: routeContext.repositoryId,
      task: routeContext.task,
      run: routeContext.run,
      candidate: `git-candidate:sha256:${'1'.repeat(64)}`,
      panel: 'candidate',
      line: '11',
    },
    panel: 'true',
    file: 'src/renamed.ts',
    line: '11',
  })
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
  assert.equal(result.initial.route.file, 'src/renamed.ts')
  assert.equal(result.initial.route.view, 'unified')
  assert.equal(result.initial.route.panel, 'candidate')
  assert.equal(result.initial.route.candidate, `git-candidate:sha256:${'1'.repeat(64)}`)
  assert.equal(result.initial.route.line, '11')
  assert.equal(result.initial.selectedLine, '11')
  assert.deepEqual(
    Object.fromEntries(Object.keys(routeContext).map(key => [key, result.initial.route[key]])),
    routeContext,
  )
  assert.equal(result.mainRegionCount, 1)

  assert.match(result.search.matchStatus, /Match 1 of 1/u)
  assert.match(result.search.activeText, /\+const kappa = 3/u)

  assert.equal(result.evidencePanel.selected, 'true')
  assert.equal(result.evidencePanel.route.panel, 'evidence')
  assert.equal(result.evidencePanel.route.file, 'src/renamed.ts')
  assert.equal(result.evidencePanel.route.line, '11')
  assert.deepEqual(
    Object.fromEntries(Object.keys(routeContext).map(key => [key, result.evidencePanel.route[key]])),
    routeContext,
  )
  assert.equal(result.lineSelection.route.line, '40')
  assert.equal(result.lineSelection.activeLine, '40')
  assert.equal(result.lineSelection.currentLine, '40')

  assert.equal(result.collapsed.focusedHunk, 'hunk:1')
  assert.match(result.collapsed.hiddenNote, /6 unchanged lines hidden/u)
  assert.equal(result.collapsed.contextRowsInFirstHunk, 0)
  assert.equal(result.collapsed.contextToggle, 'Show unchanged lines')

  assert.equal(result.switched.columns, '4')
  assert.match(result.switched.modifiedLine, /-const beta = 2/u)
  assert.match(result.switched.modifiedLine, /\+const beta = 22/u)
  assert.equal(result.switched.searchDraft, 'kappa', 'the search draft survives a layout change')
  assert.equal(result.switched.selectedPath, 'src/renamed.ts', 'the file selection is unchanged')
  assert.equal(result.switched.route.file, 'src/renamed.ts')
  assert.equal(result.switched.route.view, 'side-by-side')
  assert.deepEqual(
    Object.fromEntries(Object.keys(routeContext).map(key => [key, result.switched.route[key]])),
    routeContext,
    'layout changes preserve the historical selection and exact Scope',
  )
  assert.deepEqual(result.switched.pressed, ['unified:false', 'side-by-side:true'])
  assert.deepEqual(result.switched.calls, [['viewMode', 'side-by-side']])
  assert.equal(result.switched.stableHeaderPreserved, true,
    'layout changes retain keyed file-header nodes')
  assert.equal(result.switched.scrollTop, result.switched.scrollTopBeforeSwitch,
    'layout changes retain the real scroll position')
  assert.equal(result.switched.focusedHunkBeforeSwitch, 'hunk:1')
  assert.equal(result.switched.focusedHunkAfterSwitch, 'hunk:1',
    'layout changes keep focus on the same hunk control')
  assert.match(result.switched.focusedClassAfterSwitch, /wwc-candidate-diff-hunk-toggle/u)

  assert.equal(result.backToUnified.columns, '3')

  assert.match(result.loadedMore.status, /6 of 12 Diff lines shown\.$/u)
  assert.equal(result.loadedMore.loadMoreHidden, true)

  assert.match(result.binary.status, /Binary file preview is unavailable\./u)
  assert.equal(result.binary.rowCount, 0)
  assert.equal(result.binary.route.file, 'public/logo.png')
  assert.equal(result.binary.route.view, 'unified')
  assert.deepEqual(
    Object.fromEntries(Object.keys(routeContext).map(key => [key, result.binary.route[key]])),
    routeContext,
    'file changes preserve the historical selection and exact Scope',
  )
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
  assert.equal(result.route.view, 'unified', 'the route keeps the canonical unified value')
  assert.deepEqual(
    Object.fromEntries(Object.keys(routeContext).map(key => [key, result.route[key]])),
    routeContext,
  )
  await devtools.send('Emulation.clearDeviceMetricsOverride', {}, sessionId)
})
