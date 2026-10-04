import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.readiness-tests.json',
    '--outDir',
    'apps/client/node_modules/.cache/okqq-sidebar-tests',
    '--incremental',
    'false',
    '--pretty',
    'false',
  ],
  { cwd: root, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `okqq wiring area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, 'apps/client/node_modules/.cache/okqq-sidebar-tests')
async function cachedModule(name) {
  return import(`${pathToFileURL(resolve(cache, name)).href}?run=${String(Date.now())}`)
}

const { visibleSessions } = await cachedModule('dsh/SessionBrowser.js')
const { homeDashboardPresentation, homeDashboardAnnouncement } = await cachedModule('home-dashboard-page.js')

const session = (id, title, updatedAt, extra = {}) => ({
  id, title, updatedAt, repositoryId: 'rep_00000000000000000000000001',
  archived: false, ...extra,
})

test('history sidebar orders durable sessions without mutating the server projection', () => {
  const rows = [
    session('psn_00000000000000000000000001', '修复预览路由', '2026-10-02T08:00:00.000Z'),
    session('psn_00000000000000000000000002', '拆分三仓构建脚本', '2026-10-02T09:00:00.000Z'),
  ]
  assert.deepEqual(visibleSessions(rows, '', '', false).map(row => row.id), [rows[1].id, rows[0].id])
  assert.equal(rows[0].title, '修复预览路由')
  const updated = [{ ...rows[0], updatedAt: '2026-10-02T10:00:00.000Z' }, rows[1]]
  assert.deepEqual(visibleSessions(updated, '', '', false).map(row => row.id), [rows[0].id, rows[1].id])
})

test('history sidebar filters title, repository and archive state from durable sessions', () => {
  const first = session('psn_00000000000000000000000001', 'Preview route', '2026-10-02T08:00:00.000Z')
  const second = session('psn_00000000000000000000000002', 'Preview build', '2026-10-02T09:00:00.000Z', {
    repositoryId: 'rep_00000000000000000000000002', archived: true,
  })
  assert.deepEqual(visibleSessions([first, second], ' PREVIEW ', first.repositoryId, false), [first])
  assert.deepEqual(visibleSessions([first, second], '', second.repositoryId, true), [second])
  assert.deepEqual(visibleSessions([first, second], 'unmatched', '', false), [])
  assert.deepEqual(visibleSessions([], '', '', false), [])
})

test('board: default expand running + decisions; collapse others as count rows', () => {
  const presentation = homeDashboardPresentation()
  assert.ok(!presentation.collapsibleSections.includes('running'))
  assert.ok(!presentation.collapsibleSections.includes('decisions'))
  assert.ok(presentation.collapsibleSections.includes('backlog'))
  assert.ok(presentation.collapsibleSections.includes('ready'))
  assert.ok(presentation.collapsibleSections.includes('completed'))
  assert.equal(presentation.sectionHeading.decisions, '待我处理')
  assert.equal(presentation.sectionHeading.running, '运行中')
  assert.equal(presentation.countLabel(3), '3')
})

test('board: announcement groups home projection counts for collapsed rows', () => {
  const announcement = homeDashboardAnnouncement({
    status: 'ready',
    decisions: [],
    backlog: [{}],
    running: [{}],
    ready: [],
    waiting: [],
    validating: [],
    failed: [],
    completed: [{}],
    visited: [],
    counts: {
      decisions: 2,
      backlog: 1,
      running: 1,
      ready: 0,
      waiting: 0,
      validating: 0,
      failed: 0,
      completed: 4,
      visited: 0,
    },
    sources: { delivery: 'ok', attention: 'ok', usage: 'ok' },
    firstUse: false,
  })
  assert.match(announcement, /2 项待决策/)
  assert.match(announcement, /1 个待拆分/)
  assert.match(announcement, /1 个运行中/)
  assert.match(announcement, /4 个已完成/)
})
