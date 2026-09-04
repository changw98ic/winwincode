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
const fixturePath = 'tests/fixtures/browser-strongflow-header.mjs'

test('real Chrome shows the human next-action header and the collapsible execution identity card', async t => {
  const chromePath = chromeBinary()
  if (chromePath === null) {
    t.skip('Chrome is not installed')
    return
  }
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-strongflow-header-'))
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
  await waitForGlobal(devtools, sessionId, 'headerPrimary')

  // The first screen answers what happens now in human words and keeps every
  // technical identifier out of the primary status, reason, and next step.
  const primary = await evaluate(devtools, sessionId, 'headerPrimary()')
  assert.equal(primary.hidden, false, JSON.stringify(primary))
  assert.match(primary.status, /In progress/u)
  assert.match(primary.reason, /implementer/u)
  assert.match(primary.next, /Next step:/u)
  assert.equal(primary.hasTechnicalId, false, JSON.stringify(primary))

  // The execution identity card stays collapsed until asked, then separates
  // every identity kind onto its own labeled row with exact values.
  const identity = await evaluate(devtools, sessionId, 'headerToggleIdentity()')
  assert.equal(identity.expanded, 'true', JSON.stringify(identity))
  assert.equal(identity.listHidden, false)
  assert.equal(identity.controls, identity.listId)
  assert.equal(identity.toggleTag, 'BUTTON')
  for (const term of [
    'ProductSession',
    'StageRun',
    'Attempt',
    'ExecutionJob',
    'Worker',
    'WorkerSession',
    'CodexThread',
    'Model route',
    'Candidate',
    'Lease',
    'Events connection',
  ]) {
    assert.equal(identity.terms.includes(term), true, `missing term: ${term}`)
  }
  const values = new Map(identity.terms.map((term, index) => [term, identity.values[index]]))
  assert.equal(values.get('ProductSession'), 'psn_00000000000000000000000007')
  assert.equal(values.get('StageRun'), 'run_00000000000000000000000007')
  assert.equal(values.get('Attempt'), '3')
  assert.equal(values.get('Worker'), 'wrk_00000000000000000000000007')
  assert.equal(values.get('WorkerSession'), 'wss_00000000000000000000000007')
  assert.equal(values.get('CodexThread'), 'cdx_00000000000000000000000007')
  assert.equal(values.get('Model route'), 'Not reported')
  assert.equal(values.get('Candidate'), 'refs/winwincode/candidate/browser-header')
  assert.equal(values.get('Lease'), 'lease_00000000000000000000000007')
  assert.equal(values.get('Events connection'), 'Live events connected')

  // An equivalent snapshot keeps header and identity DOM identity while the
  // card stays open, so no reader loses their place.
  const republish = await evaluate(devtools, sessionId, 'headerEquivalentRepublish()')
  assert.deepEqual(republish, {
    sameStatus: true,
    sameList: true,
    sameRow: true,
    stillOpen: true,
    expanded: 'true',
  }, JSON.stringify(republish))

  // Blocked and failed situations announce their own human status and reason
  // while the identity card keeps reporting the exact current StageRun facts.
  const changes = await evaluate(devtools, sessionId, 'headerStateChanges()')
  assert.match(changes.waiting.status, /Waiting for your input/u, JSON.stringify(changes))
  assert.match(changes.waiting.reason, /Which repository should receive the result\?/u)
  assert.match(changes.failed.status, /Failed/u)
  assert.match(changes.failed.reason, /implementer/u)
  assert.equal(changes.waitingIdentity.join('\n'), changes.failedIdentity.join('\n'))

  assert.deepEqual(exceptions, [])
})
