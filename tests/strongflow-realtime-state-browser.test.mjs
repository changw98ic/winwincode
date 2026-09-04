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
const fixturePath = 'tests/fixtures/browser-strongflow-realtime-state-client.mjs'

test('real Chrome keeps StrongFlow review state across realtime invalidation', async t => {
  const chromePath = chromeBinary()
  if (chromePath === null) {
    t.skip('Chrome is not installed')
    return
  }
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-strongflow-realtime-state-'))
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
  await waitForGlobal(devtools, sessionId, 'runStrongFlowRealtimeStateScenario')

  const result = await evaluate(devtools, sessionId, 'runStrongFlowRealtimeStateScenario()')

  // The fixture established the review state it protects.
  assert.equal(result.runtimeBatch.before.rovingTabIndex, '0')
  assert.equal(result.runtimeBatch.before.selectedPath, 'src/module-02.ts')
  assert.equal(result.runtimeBatch.before.expandedTask, 'true')
  assert.equal(result.runtimeBatch.before.historicalStagePressed, 'true')
  assert.equal(result.runtimeBatch.before.staleNoticeHidden, true)

  // Runtime high-frequency events reset nothing.
  assert.deepEqual(result.runtimeBatch, {
    candidateRetained: true,
    taskRowRetained: true,
    stageRowRetained: true,
    taskStillExpanded: 'true',
    historicalStageStillPressed: 'true',
    selectedTab: 'candidate',
    selectedPath: 'src/module-02.ts',
    focusedStillRoving: result.runtimeBatch.before.rovingRowPath,
    rovingTabIndex: '0',
    treeScrollTop: result.runtimeBatch.before.treeScrollTop,
    diffScrollTop: result.runtimeBatch.before.diffScrollTop,
    viewportScrollTop: result.runtimeBatch.before.viewportScrollTop,
    workspaceScrollTop: result.runtimeBatch.before.workspaceScrollTop,
    zoom: result.runtimeBatch.before.zoom,
    boundaryExpanded: 'false',
    draft: 'Review draft kept in Chrome',
    caret: 7,
    staleNoticeHidden: true,
    before: result.runtimeBatch.before,
  })
  assert.equal(exceptions.length, 0, JSON.stringify(exceptions))

  // A Candidate change is reported instead of silently re-read.
  assert.equal(result.candidateChange.staleNoticeVisible, true)
  assert.equal(result.candidateChange.staleNoticeRole, 'alert')
  assert.equal(result.candidateChange.staleNoticeIconAriaHidden, 'true')
  assert.match(result.candidateChange.staleNoticeText, /Candidate changed/u)
  assert.match(result.candidateChange.staleNoticeText, /src\/module-02\.ts/u)
  assert.equal(result.candidateChange.selectedPathStill, 'src/module-02.ts')
  assert.equal(result.candidateChange.focusDroppedToBody, false)
  assert.equal(result.candidateChange.candidateRetained, true)
  assert.equal(result.candidateChange.selectedTab, 'candidate')
  assert.equal(result.candidateChange.expandedTask, 'true')
  assert.equal(result.candidateChange.treeScrollTop, result.runtimeBatch.before.treeScrollTop)
  assert.equal(result.candidateChange.diffScrollTop, result.runtimeBatch.before.diffScrollTop)

  // Selecting a file again is the explicit re-confirmation.
  assert.equal(result.reconfirmed.noticeHidden, true)
  assert.equal(result.reconfirmed.selectedPath, 'src/module-05.ts')

  // No open Diff means there is nothing to re-confirm.
  assert.equal(result.withoutReviewContext.noticeHidden, true)
})
