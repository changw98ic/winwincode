import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.strongflow-page-tests.json',
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
  `StrongFlow page did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const preferences = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-page-tests/strongflow-layout-preferences.js',
)).href}`)

const {
  DEFAULT_STRONGFLOW_LAYOUT,
  normalizeStrongFlowLayoutPreferences,
  strongFlowLayoutPreferencesFromStorage,
  strongFlowLayoutPreferencesToStorage,
} = preferences

class FakeStorage {
  #entries = new Map()
  #failOn = null

  constructor(failOn = null) {
    this.#failOn = failOn
  }

  get size() {
    return this.#entries.size
  }

  getItem(key) {
    if (this.#failOn === 'getItem') throw new Error('storage unavailable')
    return this.#entries.get(key) ?? null
  }

  setItem(key, value) {
    if (this.#failOn === 'setItem') throw new Error('storage unavailable')
    this.#entries.set(key, String(value))
  }

  removeItem(key) {
    if (this.#failOn === 'removeItem') throw new Error('storage unavailable')
    this.#entries.delete(key)
  }
}

test('layout preferences default to an expanded three-pane desktop workbench', () => {
  assert.deepEqual(DEFAULT_STRONGFLOW_LAYOUT, {
    navigationWidth: 22,
    contextWidth: 30,
    navigationCollapsed: false,
    contextCollapsed: false,
    artifactsTab: 'solution',
  })
})

test('normalization clamps pane widths and canonicalizes tabs and collapse flags', () => {
  assert.deepEqual(normalizeStrongFlowLayoutPreferences(null), DEFAULT_STRONGFLOW_LAYOUT)
  assert.deepEqual(normalizeStrongFlowLayoutPreferences('not-an-object'), DEFAULT_STRONGFLOW_LAYOUT)
  assert.deepEqual(normalizeStrongFlowLayoutPreferences({
    navigationWidth: 999,
    contextWidth: -5,
    navigationCollapsed: 'yes',
    contextCollapsed: 0,
    artifactsTab: 'evidence',
  }), {
    navigationWidth: 45,
    contextWidth: 18,
    navigationCollapsed: true,
    contextCollapsed: false,
    artifactsTab: 'evidence',
  })
  assert.deepEqual(normalizeStrongFlowLayoutPreferences({
    navigationWidth: 12,
    artifactsTab: 'unknown-tab',
  }), {
    ...DEFAULT_STRONGFLOW_LAYOUT,
    navigationWidth: 18,
  })
})

test('preferences round-trip through localStorage under one canonical key', () => {
  const storage = new FakeStorage()
  const value = normalizeStrongFlowLayoutPreferences({ navigationWidth: 25 })
  strongFlowLayoutPreferencesToStorage(storage, value)
  assert.equal(storage.size, 1)
  assert.deepEqual(
    strongFlowLayoutPreferencesFromStorage(storage),
    value,
  )
})

test('storage failures and corrupt payloads fall back to the default layout', () => {
  assert.deepEqual(strongFlowLayoutPreferencesFromStorage(null), DEFAULT_STRONGFLOW_LAYOUT)
  const failingRead = new FakeStorage('getItem')
  assert.deepEqual(strongFlowLayoutPreferencesFromStorage(failingRead), DEFAULT_STRONGFLOW_LAYOUT)
  const failingWrite = new FakeStorage('setItem')
  strongFlowLayoutPreferencesToStorage(failingWrite, DEFAULT_STRONGFLOW_LAYOUT)
  assert.equal(failingWrite.size, 0)
  const corrupt = new FakeStorage()
  corrupt.setItem('winwincode.strongflow.layout.v1', '{not json')
  assert.deepEqual(strongFlowLayoutPreferencesFromStorage(corrupt), DEFAULT_STRONGFLOW_LAYOUT)
})

test('toStorage removes the key when the value equals the default layout', () => {
  const storage = new FakeStorage()
  strongFlowLayoutPreferencesToStorage(storage, DEFAULT_STRONGFLOW_LAYOUT)
  assert.equal(storage.size, 0)
  strongFlowLayoutPreferencesToStorage(storage, { ...DEFAULT_STRONGFLOW_LAYOUT, navigationWidth: 30 })
  assert.equal(storage.size, 1)
  strongFlowLayoutPreferencesToStorage(storage, DEFAULT_STRONGFLOW_LAYOUT)
  assert.equal(storage.size, 0)
})
