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
    'apps/client/tsconfig.candidate-run-preview-tests.json',
    '--pretty',
    'false',
    '--incremental',
    'false',
  ],
  { cwd: root, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `okqq wiring area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/candidate-run-preview-tests')
async function cachedModule(name) {
  return import(`${pathToFileURL(resolve(cache, name)).href}?run=${String(Date.now())}`)
}

const recentChats = await cachedModule('recent-chats.js')
const homePresentation = await cachedModule('home-dashboard-page.js')

const {
  loadRecentChats,
  recordRecentChat,
  RECENT_CHATS_LIMIT,
  RECENT_CHATS_STORAGE_KEY,
} = recentChats
const { homeDashboardPresentation, homeDashboardAnnouncement } = homePresentation

function memoryStorage(seed = null) {
  const map = new Map()
  if (seed !== null) map.set(seed.key, seed.value)
  return {
    getItem(key) {
      return map.get(key) ?? null
    },
    setItem(key, value) {
      map.set(key, value)
    },
  }
}

test('recent chats: opening sessions writes shell sidebar source', () => {
  const storage = memoryStorage()
  const events = []
  globalThis.window = {
    dispatchEvent(event) {
      events.push(event.type)
      return true
    },
  }
  try {
    recordRecentChat(storage, {
      sessionKey: 'psn_00000000000000000000000001',
      title: '修复预览路由',
      at: 1_700_000_000_000,
    })
    recordRecentChat(storage, {
      sessionKey: 'psn_00000000000000000000000002',
      title: '拆分三仓构建脚本',
      at: 1_700_000_001_000,
    })
    // Re-open the first session: it moves to the front, no duplicate key.
    recordRecentChat(storage, {
      sessionKey: 'psn_00000000000000000000000001',
      title: '修复预览路由',
      at: 1_700_000_002_000,
    })
    const entries = loadRecentChats(storage)
    assert.equal(entries.length, 2)
    assert.equal(entries[0].sessionKey, 'psn_00000000000000000000000001')
    assert.equal(entries[0].title, '修复预览路由')
    assert.equal(entries[1].sessionKey, 'psn_00000000000000000000000002')
    assert.ok(events.includes('wwc:recent-chats-changed'))
    const raw = JSON.parse(storage.getItem(RECENT_CHATS_STORAGE_KEY))
    assert.equal(raw.length, 2)
  } finally {
    delete globalThis.window
  }
})

test('recent chats: list stays bounded and empty storage is safe', () => {
  const storage = memoryStorage()
  for (let index = 0; index < RECENT_CHATS_LIMIT + 3; index += 1) {
    recordRecentChat(storage, {
      sessionKey: `psn_${String(index).padStart(26, '0')}`,
      title: `会话 ${String(index)}`,
      at: 1_700_000_000_000 + index,
    })
  }
  const entries = loadRecentChats(storage)
  assert.equal(entries.length, RECENT_CHATS_LIMIT)
  assert.equal(entries[0].title, `会话 ${String(RECENT_CHATS_LIMIT + 2)}`)
  assert.deepEqual(loadRecentChats(null), [])
})

test('board: default expand running + decisions; collapse others as count rows', () => {
  const presentation = homeDashboardPresentation()
  assert.ok(!presentation.collapsibleSections.includes('running'))
  assert.ok(!presentation.collapsibleSections.includes('decisions'))
  assert.ok(presentation.collapsibleSections.includes('backlog'))
  assert.ok(presentation.collapsibleSections.includes('ready'))
  assert.ok(presentation.collapsibleSections.includes('completed'))
  assert.equal(presentation.sectionHeading.decisions, '待我处理')
  assert.equal(presentation.sectionHeading.running, '运行中（Running）')
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
