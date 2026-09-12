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
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-home-dashboard.mjs',
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  const clientOrigin = `https://client.localhost:${String(clientPort)}`
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

  // The start-up route carries no product path at all, and this identity has
  // several authorized repository Scopes: the shell must ask for an exact Scope
  // instead of opening an arbitrary Chat or the first Delivery.
  await devtools.send('Page.navigate', { url: clientOrigin }, sessionId)
  await waitForGlobal(devtools, sessionId, 'homeReady')
  await waitForGlobal(devtools, sessionId, 'inspectLanding')
  const landing = await evaluateInBrowser('globalThis.inspectLanding()')
  // 设计稿 03a:新对话是默认落地页。
  assert.equal(landing.surface, 'chat', JSON.stringify(landing))
  assert.equal(landing.hash === '' || landing.hash === '#/home', true, landing.hash)
  assert.equal(landing.present, false, 'no dashboard mounts without an exact Scope')
  assert.equal(landing.leak, false)
  const landingText = await evaluateInBrowser(
    "document.body.textContent.replace(/\\s+/gu, ' ')",
  )
  assert.match(landingText, /选择一个已授权的仓库范围以打开工作区/u)
  assert.doesNotMatch(landingText, /Conversation workspace/u)

  // With one exact Scope in the URL, the dashboard is the first screen.
  const home = await evaluateInBrowser(`globalThis.openHome('#/home?organizationId=${identity}`
    + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryOne}')`)
  assert.equal(home.liveRegions, 1, 'the dashboard keeps exactly one polite live region')
  assert.match(home.status, /就绪 ·/u)
  // WWC-ER-1001 board order: the two live columns, then the collapsed rows.
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
  // The executing Delivery carries failed task counts, so its canonical
  // section is 失败或阻塞 rather than 正在运行.
  assert.equal(section('running').cards.length, 0)
  assert.equal(section('failed').cards.length, 1)
  assert.equal(section('completed').cards.length, 1)
  assert.equal(section('decisions').cards[0].title, 'Review the proposed delivery scope')
  assert.match(section('failed').cards[0].title, /repository 1/u)
  assert.equal(home.firstUse.hidden, true)
  // Design page 04 removed the usage/health panel from the board.
  assert.equal(home.usage.present, false)
  assert.deepEqual(home.unavailableNotes, [])

  // Every actionable card opens its exact, Scope-complete deep link; Delivery
  // and business-Attention cards render no dead-end action link at all.
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
  // Delivery review cards open the exact Delivery-bound review surface.
  assert.equal(
    decisionByTitle('Review the proposed delivery scope')?.action.href,
    `#/home/review?delivery=dlv_00000000000000000000000001&organizationId=${identity}`
      + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryOne}`,
    JSON.stringify(decisionCards),
  )
  // 设计稿 04:交付卡的动作是「查看进度」,打开运行页。
  const runHref = `#/home/task-run?organizationId=${identity}`
    + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryOne}`
  assert.equal(section('failed').cards[0]?.action.href, runHref)
  assert.equal(
    home.chatLinks.filter(href => href === `#/chat?session=psn_00000000000000000000000001&${scoped}`).length,
    1,
    JSON.stringify(home.chatLinks),
  )

  // A Scope switch re-reads every projection and re-renders in isolation.
  const switched = await evaluateInBrowser('globalThis.switchRepositoryScope()')
  // Every projection re-read the new Scope after the switch.
  assert.deepEqual(switched.scopedQueries.slice(-2), [repositoryTwo, repositoryTwo])
  assert.ok(switched.scopedQueries.includes(repositoryOne), switched.scopedQueries.join(' '))
  assert.match(JSON.stringify(switched.afterSectionTitles), /repository 2/u)
  assert.doesNotMatch(JSON.stringify(switched.afterSectionTitles), /repository 1/u)
  assert.equal(switched.leak, false)
  const switchedDashboard = await evaluateInBrowser('globalThis.readDashboard()')
  assert.match(switchedDashboard.status, /就绪 ·/u)
  // repositoryTwo's Delivery is verifying with failed task counts, so the
  // canonical section is 失败或阻塞 and 正在运行 stays empty.
  assert.equal(
    switchedDashboard.sections.find(section => section.id === 'running')?.cards.length,
    0,
  )
  assert.equal(
    switchedDashboard.sections.find(section => section.id === 'failed')?.cards.length,
    1,
  )
  // The switched Scope's cards keep the 查看进度 action scoped to the new repo.
  const switchedRunHref = `#/home/task-run?organizationId=${identity}`
    + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryTwo}`
  assert.equal(
    switchedDashboard.sections.find(section => section.id === 'failed')
      ?.cards[0]?.action.href,
    switchedRunHref,
  )
  assert.equal(switchedDashboard.liveRegions, 1)

  // Design page 04: an unused Scope shows the 新建任务 entry instead of the
  // old first-use delivery/chat link block. The repositoryThree state has no
  // deliveries, so openHome's cards-wait does not apply here.
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
