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

  // The page loads the exact Task/Attempt association named by the deep link.
  const deepLink = await evaluate(devtools, sessionId, 'historyDeepLinkSnapshot()')
  assert.equal(deepLink.detailVisible, true, JSON.stringify(deepLink))
  assert.equal(deepLink.taskExpanded, 'true')
  assert.equal(deepLink.pressedRun, failedRunId)
  assert.match(deepLink.detailText, new RegExp(failedRunId, 'u'))
  assert.match(deepLink.detailText, /Attempt 1/u)
  assert.match(deepLink.detailText, /psn_00000000000000000000000001/u)
  assert.match(deepLink.detailText, /refs\/winwincode\/candidate\/attempt-1/u)
  assert.equal(deepLink.hash, `${baseHash}&task=task%3Abrowser&run=${failedRunId}`)

  // Historical review fails closed: current Delivery mutations stay disabled
  // and clicking them issues no command.
  const gate = await evaluate(devtools, sessionId, 'historyMutationGate()')
  assert.equal(gate.advanceDisabled, true, JSON.stringify(gate))
  assert.equal(gate.noteVisible, true)
  assert.equal(gate.advanceCalls, 0)

  // The exact historical RuntimeProjection renders, bounded to 100 of 200
  // runtime events with an explicit omitted count.
  const runtime = await evaluate(devtools, sessionId, 'historyRuntimeProbe()')
  assert.equal(runtime.sessionCount, 1, JSON.stringify(runtime))
  assert.equal(runtime.activityCount, 100)
  assert.match(runtime.runtimeText, /Runtime revision 7/u)
  assert.match(runtime.sessionText, /cdx_00000000000000000000000001/u)
  assert.match(runtime.activityText, /cargo test --event 1/u)
  assert.match(runtime.omittedText, /100 more runtime activities not shown/u)

  // The historical Candidate opens as a display-only review.
  const review = await evaluate(devtools, sessionId, 'historyOpenCandidate()')
  assert.equal(review.expanded, 'true', JSON.stringify(review))
  assert.match(review.reviewText, /4444444444444444444444444444444444444444/u)
  assert.match(review.reviewText, /r5/u)
  assert.match(review.noteText, /never authorizes/u)

  // An equivalent snapshot keeps detail DOM identity, focus, and scroll.
  const probe = await evaluate(devtools, sessionId, 'historyIdentityProbe()')
  assert.equal(probe.focusedClass, 'wwc-strongflow-history-candidate')
  const identity = await evaluate(devtools, sessionId, 'historyEquivalentRepublish()')
  assert.deepEqual(identity, {
    sameDetail: true,
    sameEvidence: true,
    sameActivity: true,
    focusPreserved: true,
    scrollPreserved: true,
    activityCount: 100,
  }, JSON.stringify(identity))

  const keyboard = await evaluate(devtools, sessionId, 'historyKeyboardFlow()')
  assert.equal(keyboard.before.focusClass, 'wwc-strongflow-history-toggle')
  assert.equal(keyboard.before.expanded, 'true')
  assert.equal(keyboard.afterExpand.expanded, 'true')
  assert.equal(keyboard.afterExpand.focusClass, 'wwc-strongflow-run-button')
  assert.equal(keyboard.afterExpand.focusRun, failedRunId)
  assert.equal(keyboard.afterDown.focusRun, currentRunId)
  assert.equal(keyboard.afterDown.focusClass, 'wwc-strongflow-current-run')

  // Timeline ArrowLeft must never collapse the separate Task tree.
  const arrow = await evaluate(devtools, sessionId, 'historyTimelineArrowLeft()')
  assert.equal(arrow.expanded, 'true', JSON.stringify(arrow))
  assert.equal(arrow.focusRun, failedRunId)

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
  assert.match(humanReview.detailText, /No runtime projection — this StageRun has no runtime binding\./u)

  // A full browser reload remounts the page from the same URL and must restore
  // the selected historical run instead of resetting to the live view.
  await evaluate(devtools, sessionId, 'historySelectTimelineRun()')
  await devtools.send('Page.navigate', { url: `${clientOrigin}/${baseHash}&run=${planningRunId}` }, sessionId)
  await waitForGlobal(devtools, sessionId, 'historyDeepLinkSnapshot')
  const restored = await evaluate(devtools, sessionId, 'historyRestoreAfterReload()')
  assert.equal(restored.detailVisible, true, JSON.stringify(restored))
  assert.equal(restored.pressedRun, planningRunId)
  assert.match(restored.detailText, new RegExp(planningRunId, 'u'))

  const current = await evaluate(devtools, sessionId, 'historyReturnToCurrent()')
  assert.equal(current.detailVisible, false, JSON.stringify(current))
  assert.equal(current.pressedRun, null)
  assert.equal(current.hash, baseHash)

  // Returning to the current run restores the mutation controls.
  const restoredGate = await evaluate(devtools, sessionId, 'historyMutationGate()')
  assert.equal(restoredGate.advanceDisabled, false, JSON.stringify(restoredGate))
  assert.equal(restoredGate.noteVisible, false)
  assert.equal(restoredGate.advanceCalls, 1)

  assert.deepEqual(exceptions, [])
})
