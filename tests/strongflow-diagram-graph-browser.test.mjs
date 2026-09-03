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
const fixturePath = 'tests/fixtures/browser-strongflow-diagram-graph.mjs'

test('real Chrome renders interactive solution graphs that survive runtime snapshots', async t => {
  const chromePath = chromeBinary()
  if (chromePath === null) {
    t.skip('Chrome is not installed')
    return
  }
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-strongflow-diagram-'))
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
  await waitForGlobal(devtools, sessionId, 'runDiagramGraphScenario')

  const scenario = await evaluate(devtools, sessionId, 'runDiagramGraphScenario()')
  assert.equal(scenario.viewport.role, 'group')
  assert.equal(scenario.viewport.mode, 'wide')
  assert.match(scenario.viewport.label, /Architecture graph viewport/u)
  assert.equal(scenario.stateChips.deliveryStatus, 'executing')
  assert.match(scenario.stateChips.text, /Delivery executing · solution review pending/u)

  assert.equal(scenario.nodes.count, 4)
  const positions = scenario.nodes.positions
  assert.deepEqual(positions.map(position => position.id), [
    'platform:dsh',
    'platform:strongflow',
    'component:delivery-api',
    'platform:codex-core',
  ])
  for (let index = 1; index < positions.length; index += 1) {
    assert.ok(
      positions[index].x > positions[index - 1].x,
      `rank ${String(index)} renders further right: ${JSON.stringify(positions)}`,
    )
  }
  assert.deepEqual(scenario.nodes.unresolved, {
    badge: 'Unresolved',
    color: 'rgb(254, 242, 242)',
    icon: '▣',
  })

  assert.deepEqual(scenario.edges.map(edge => edge.id), [
    'edge:dsh-submit',
    'edge:control-api',
    'edge:api-exec',
  ])
  assert.equal(scenario.edgeGeometryConnected, true, (
    'winwincode-zs0: a branched visual edge must span the source-to-target gap'
  ))
  assert.equal(scenario.edges[1].ariaLabel, 'WinWinCode → Delivery API: calls')
  assert.deepEqual(scenario.overview, {
    label: 'Fit Architecture overview, 4 nodes and 3 connections',
    nodes: 4,
  })

  assert.deepEqual(scenario.keyboardFocus, { id: 'platform:strongflow' })
  assert.equal(scenario.selection.ariaPressed, 'true')
  assert.match(
    scenario.selection.label,
    /Delivery API, component, Delivery control plane, Unresolved/u,
  )
  assert.match(scenario.detail, /Delivery control plane/u)
  assert.match(scenario.detail, /Unresolved/u)

  assert.equal(scenario.viewport.zoom, '1.25')
  assert.match(scenario.viewport.transform, /^matrix\(1\.25/u)

  assert.equal(scenario.listEquivalent.hidden, false)
  assert.equal(scenario.listEquivalent.rowCount, 4)
  assert.match(scenario.listEquivalent.rowText, /Unresolved/u)
  assert.match(scenario.listEquivalent.rowText, /WinWinCode → Delivery API: calls/u)

  assert.deepEqual(scenario.boundary, {
    collapsed: 'false',
    chipVisible: true,
    memberHidden: true,
  })

  const stability = await evaluate(devtools, sessionId, 'runDiagramStabilityScenario()')
  assert.deepEqual(stability, {
    focusKept: true,
    graphMutations: 0,
    nodeIdentity: true,
    pressedKept: true,
    zoomKept: '1.25',
  })

  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 420,
    height: 900,
    deviceScaleFactor: 1,
    mobile: true,
  }, sessionId)
  const narrowMode = await evaluate(
    devtools,
    sessionId,
    'waitForDiagramViewportMode("narrow")',
  )
  assert.equal(narrowMode, 'narrow')
  const narrowEdgeConnected = await evaluate(
    devtools,
    sessionId,
    'measureDiagramEdgeGeometry()',
  )
  assert.equal(narrowEdgeConnected, true, 'narrow edges must retain source-to-target geometry')
  const narrowPageFits = await evaluate(
    devtools,
    sessionId,
    'document.documentElement.scrollWidth <= window.innerWidth',
  )
  assert.equal(narrowPageFits, true, 'narrow graph controls must not overflow the page')
  assert.equal(exceptions.length, 0, JSON.stringify(exceptions))
})
