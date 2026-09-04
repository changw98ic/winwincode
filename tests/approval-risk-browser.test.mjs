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
const fixturePath = 'tests/fixtures/browser-approval-risk-client.mjs'
const commandLimit = 200

test('real Chrome shows the approval risk detail, scope, and impact before a decision', async t => {
  const chromePath = chromeBinary()
  if (chromePath === null) {
    t.skip('Chrome is not installed')
    return
  }
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-approval-risk-'))
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
  t.after(async () => {
    devtools.close()
    await Promise.all([stopChild(chrome, 'SIGTERM'), closeServer(clientServer)])
    rmSync(directory, { recursive: true, force: true })
    assert.equal(existsSync(directory), false)
  })
  devtools.on('Runtime.exceptionThrown', event => { exceptions.push(event) })
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  const { sessionId } = await devtools.send('Target.attachToTarget', {
    targetId,
    flatten: true,
  })
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runApprovalRiskScenario')

  const result = await evaluate(devtools, sessionId, 'runApprovalRiskScenario()')

  // One risk block per approval card, and the decision form is enabled for the
  // two current approvals.
  assert.equal(result.riskBlockCount, 3)
  assert.deepEqual(result.decisions.slice(0, 2).map(node => node.disabled), [false, false])
  for (const decision of result.decisions.slice(0, 2)) {
    assert.equal(decision.label, 'Approve')
  }

  // What will run: a bounded, single-line producer summary only.
  assert.equal(result.shell.commandLength, commandLimit)
  assert.equal(result.shell.subjectLength, commandLimit)
  assert.equal(result.shell.leaksRawCommand, false)
  assert.equal(result.shell.leaksToken, false)
  assert.equal(result.shell.impact, 'Shell execution')
  assert.equal(result.shell.level, 'High risk')

  // Where it runs, and that approving once never broadens the scope.
  assert.match(result.shell.scope, /Approve once/u)
  assert.match(result.shell.scope, /never extends to the Worker session/u)
  assert.match(result.shell.target, /WorkerSession-bound/u)
  assert.match(result.shell.expiry, /Expires/u)
  assert.equal(result.shell.commandBeforeDecisionForm, true)

  // Fields the secret-safe projection withholds degrade explicitly.
  assert.equal(result.fieldKeyCount, 18)
  for (const key of ['cwd', 'fileImpact', 'networkTargets', 'mcpTarget', 'requestedReason']) {
    assert.equal(result.withheld[key].length, 3, key)
    for (const text of result.withheld[key]) {
      assert.match(text, /Withheld|Not reported|Not recorded/u, `${key}: ${text}`)
    }
  }

  // Unknown actions and expired approvals degrade instead of guessing.
  assert.equal(result.mcpImpact, 'MCP tool call')
  assert.equal(result.unclassified.level, 'Risk unknown')
  assert.equal(result.unclassified.impact, 'Unclassified action')
  assert.equal(result.unclassified.expiry.includes('Expired'), true)
  assert.equal(result.unclassified.cardState, 'expired')
  assert.equal(result.unclassified.approveDisabled, true)

  assert.equal(exceptions.length, 0, JSON.stringify(exceptions))
})
