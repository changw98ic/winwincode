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
const fixturePath = 'tests/fixtures/browser-strongflow-history.mjs'

const deliveryId = 'dlv_00000000000000000000000001'
const failedRunId = 'run_00000000000000000000000001'
const currentRunId = 'run_00000000000000000000000002'
const planningRunId = 'run_00000000000000000000000003'
const reviewRunId = 'run_00000000000000000000000004'
const baseHash = `#/strongflow?delivery=${deliveryId}&session=psn_00000000000000000000000002&stageRun=${currentRunId}`

test('real Chrome restores, navigates, and reviews StrongFlow history through the URL', async t => {
  const chromePath = chromeBinary()
  if (chromePath === null) {
    t.skip('Chrome is not installed')
    return
  }
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-strongflow-history-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath,
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  const clientOrigin = `https://client.localhost:${String(clientPort)}`
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
    await Promise.all([
      stopChild(chrome, 'SIGTERM'),
      closeServer(clientServer),
    ])
    rmSync(directory, { recursive: true, force: true })
    assert.equal(existsSync(directory), false)
  })
  devtools.on('Runtime.exceptionThrown', event => { exceptions.push(event) })
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  ;({ sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true }))
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 1440,
    height: 1000,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  await devtools.send('Page.navigate', { url: clientOrigin }, sessionId)
  await waitForGlobal(devtools, sessionId, 'historyDeepLinkSnapshot')

  const deepLink = await evaluate(devtools, sessionId, 'historyDeepLinkSnapshot()')
  assert.equal(deepLink.detailVisible, true, JSON.stringify(deepLink))
  assert.equal(deepLink.taskExpanded, 'true')
  assert.equal(deepLink.pressedRun, failedRunId)
  assert.match(deepLink.detailText, new RegExp(failedRunId, 'u'))
  assert.match(deepLink.detailText, /Attempt 1/u)
  assert.match(deepLink.detailText, /psn_00000000000000000000000001/u)
  assert.match(deepLink.detailText, /refs\/winwincode\/candidate\/attempt-1/u)
  assert.equal(deepLink.hash, `${baseHash}&task=task%3Abrowser&run=${failedRunId}`)

  const keyboard = await evaluate(devtools, sessionId, 'historyKeyboardFlow()')
  assert.equal(keyboard.before.focusClass, 'wwc-strongflow-history-toggle')
  assert.equal(keyboard.before.expanded, 'true')
  assert.equal(keyboard.afterExpand.expanded, 'true')
  assert.equal(keyboard.afterExpand.focusClass, 'wwc-strongflow-run-button')
  assert.equal(keyboard.afterExpand.focusRun, failedRunId)
  assert.equal(keyboard.afterDown.focusRun, currentRunId)
  assert.equal(keyboard.afterDown.focusClass, 'wwc-strongflow-current-run')

  const planning = await evaluate(devtools, sessionId, 'historySelectTimelineRun()')
  assert.equal(planning.detailVisible, true, JSON.stringify(planning))
  assert.equal(planning.pressedRun, planningRunId)
  assert.equal(planning.hash, `${baseHash}&run=${planningRunId}`)
  assert.match(planning.detailText, new RegExp(planningRunId, 'u'))
  assert.match(planning.detailText, /planning/u)
  assert.match(planning.detailText, /psn_00000000000000000000000003/u)

  const humanReview = await evaluate(devtools, sessionId, 'historySelectHumanReviewRun()')
  assert.equal(humanReview.detailVisible, true, JSON.stringify(humanReview))
  assert.equal(humanReview.pressedRun, reviewRunId)
  assert.equal(humanReview.hash, `${baseHash}&run=${reviewRunId}`)
  assert.match(humanReview.detailText, /plan-review/u)
  assert.match(humanReview.detailText, /Human review StageRun — no runtime binding\./u)

  // A full browser reload remounts the page from the same URL and must restore
  // the selected historical run instead of resetting to the live view.
  await evaluate(devtools, sessionId, 'historySelectTimelineRun()')
  await devtools.send('Page.navigate', { url: `${clientOrigin}/${baseHash}&run=${planningRunId}` }, sessionId)
  await waitForGlobal(devtools, sessionId, 'historyDeepLinkSnapshot')
  const restored = await evaluate(devtools, sessionId, 'historyDeepLinkSnapshot()')
  assert.equal(restored.detailVisible, true, JSON.stringify(restored))
  assert.equal(restored.pressedRun, planningRunId)
  assert.match(restored.detailText, new RegExp(planningRunId, 'u'))

  const current = await evaluate(devtools, sessionId, 'historyReturnToCurrent()')
  assert.equal(current.detailVisible, false, JSON.stringify(current))
  assert.equal(current.pressedRun, null)
  assert.equal(current.hash, baseHash)

  assert.deepEqual(exceptions, [])
})
