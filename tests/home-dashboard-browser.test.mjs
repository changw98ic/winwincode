import assert from 'node:assert/strict'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
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
const identity = 'org_00000000000000000000000001'
const workspaceId = 'wsp_00000000000000000000000001'
const projectId = 'prj_00000000000000000000000001'
const repositoryOne = 'rep_00000000000000000000000001'
const repositoryTwo = 'rep_00000000000000000000000002'
const repositoryThree = 'rep_00000000000000000000000003'
const SECRET_MARKER = 'vault-locator-secret-marker'

test('a real browser opens 新对话 as the first screen and the board one click away', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the Home browser test')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-home-dashboard-'))
  const clientServer = staticClientServer({
    root,
    fixturePath: 'tests/fixtures/browser-home-dashboard.mjs',
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  const clientOrigin = `http://127.0.0.1:${String(clientPort)}`
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
  const evaluateInBrowser = async expression => evaluate(devtools, sessionId, expression)

  await devtools.send('Page.navigate', { url: clientOrigin }, sessionId)
  await waitForGlobal(devtools, sessionId, 'homeReady')
  await waitForGlobal(devtools, sessionId, 'inspectLanding')
  const landing = await evaluateInBrowser('globalThis.inspectLanding()')
  assert.equal(landing.surface, 'chat', JSON.stringify(landing))
  assert.equal(landing.hash === '' || landing.hash === '#/home', true, landing.hash)
  assert.equal(landing.present, false, 'no dashboard mounts without an exact Scope')
  assert.equal(landing.leak, false)
  const landingText = await evaluateInBrowser(
    "document.body.textContent.replace(/\\s+/gu, ' ')",
  )
  assert.match(landingText, /选择一个已授权的仓库范围以打开工作区/u)
  assert.doesNotMatch(landingText, /Conversation workspace/u)

  const home = await evaluateInBrowser(`globalThis.openHome('#/home?organizationId=${identity}`
    + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryOne}')`)
  assert.equal(home.liveRegions, 1, 'the dashboard keeps exactly one polite live region')
  assert.match(home.status, /就绪 ·/u)
  assert.deepEqual(home.sections.map(section => section.id), [
    'running',
    'decisions',
    'backlog',
    'ready',
    'waiting',
    'validating',
    'failed',
    'completed',
    'visited',
  ])
  const sectionOf = dashboard => id => dashboard.sections.find(
    candidate => candidate.id === id,
  )
  const section = sectionOf(home)
  assert.equal(section('decisions').cards.length, 2, JSON.stringify(home.sections))
  assert.equal(section('running').cards.length, 0)
  assert.equal(section('failed').cards.length, 1)
  assert.equal(section('completed').cards.length, 1)
  assert.equal(section('decisions').cards[0].title, 'Delivery of repository 1')
  assert.match(section('failed').cards[0].title, /repository 1/u)
  assert.equal(home.firstUse.hidden, true)
  assert.equal(home.usage.present, false)
  assert.deepEqual(home.unavailableNotes, [])

  const scoped = `organizationId=${identity}&workspaceId=${workspaceId}`
    + `&projectId=${projectId}&repositoryId=${repositoryOne}`
  const chatHref = `#/chat?session=psn_00000000000000000000000001&${scoped}`
  const decisionCards = section('decisions').cards
  const decisionByTitle = title => decisionCards.find(card => card.title === title)
  for (const action of home.actions) {
    if (action.href === null) continue
    assert.match(action.href, new RegExp(`repositoryId=${repositoryOne}$`), action.href)
    assert.equal(action.disabled, null, action.href)
  }
  assert.equal(
    decisionByTitle('Allow the projected repository action')?.action.href,
    chatHref,
    JSON.stringify(decisionCards),
  )
  assert.equal(
    decisionByTitle('Delivery of repository 1')?.action.href,
    `#/home/review?delivery=dlv_00000000000000000000000001&organizationId=${identity}`
      + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryOne}`,
    JSON.stringify(decisionCards),
  )
  const runHref = `#/home/review?delivery=dlv_00000000000000000000000001&organizationId=${identity}`
    + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryOne}`
  assert.equal(section('failed').cards[0]?.action.href, runHref)
  assert.equal(
    home.chatLinks.filter(href => href === `#/chat?session=psn_00000000000000000000000001&${scoped}`).length,
    1,
    JSON.stringify(home.chatLinks),
  )

  const switched = await evaluateInBrowser('globalThis.switchRepositoryScope()')
  assert.deepEqual(switched.scopedQueries.slice(-2), [repositoryTwo, repositoryTwo])
  assert.ok(switched.scopedQueries.includes(repositoryOne), switched.scopedQueries.join(' '))
  assert.match(JSON.stringify(switched.afterSectionTitles), /repository 2/u)
  assert.doesNotMatch(JSON.stringify(switched.afterSectionTitles), /repository 1/u)
  assert.equal(switched.leak, false)
  const switchedDashboard = await evaluateInBrowser('globalThis.readDashboard()')
  assert.match(switchedDashboard.status, /就绪 ·/u)
  assert.equal(
    switchedDashboard.sections.find(section => section.id === 'running')?.cards.length,
    0,
  )
  assert.equal(
    switchedDashboard.sections.find(section => section.id === 'failed')?.cards.length,
    1,
  )
  const switchedRunHref = `#/home/review?delivery=dlv_00000000000000000000000002&organizationId=${identity}`
    + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryTwo}`
  assert.equal(
    switchedDashboard.sections.find(section => section.id === 'failed')
      ?.cards[0]?.action.href,
    switchedRunHref,
  )
  assert.equal(switchedDashboard.liveRegions, 1)

  await evaluateInBrowser(`location.hash = '#/home?organizationId=${identity}`
    + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryThree}'`)
  const emptyScope = await evaluateInBrowser(`new Promise(resolvePromise => {
    const deadline = Date.now() + 10_000
    const check = () => {
      const page = document.querySelector('.wwc-home')
      const status = document.querySelector('.wwc-home-status')?.textContent ?? ''
      if (page !== null && status.includes('就绪')) {
        resolvePromise({
          firstUse: { hidden: document.querySelector('.wwc-home-first-use')?.hidden ?? true },
          leak: document.body.textContent.includes('SECRET_MARKER'),
        })
        return
      }
      if (Date.now() >= deadline) {
        resolvePromise({ firstUse: { hidden: null }, leak: false, status })
        return
      }
      setTimeout(check, 50)
    }
    check()
  })`)
  assert.equal(emptyScope.firstUse.hidden, true, JSON.stringify(emptyScope))
  assert.equal(emptyScope.leak, false)
})
