// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { mkdirSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const out = resolve(root, '.cache/repository-display-labels-test')
mkdirSync(out, { recursive: true })
execFileSync('corepack', ['pnpm', 'exec', 'tsc', 'apps/client/src/display-labels.ts', '--ignoreConfig', '--target', 'ES2023', '--module', 'NodeNext', '--moduleResolution', 'NodeNext', '--outDir', out, '--skipLibCheck', '--pretty', 'false'], { cwd: root })
const labels = await import(`${resolve(out, 'display-labels.js')}?test=${Date.now()}`)

function browser() {
  const values = new Map()
  return {
    localStorage: { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value) },
    dispatchEvent() {},
  }
}

test('repository display names save, read, restore, and report storage failures', () => {
  const win = browser()
  const id = 'rbd_1'
  assert.equal(labels.repositoryDisplayName('rep_00000000000000000000000001', id, win), '项目名称未设置')
  labels.saveRepositoryDisplayName(win, id, '我的项目')
  assert.equal(labels.repositoryDisplayName('原始名称', id, win), '我的项目')
  labels.clearRepositoryDisplayName(win, id)
  assert.equal(labels.repositoryDisplayName('原始名称', id, win), '原始名称')
  assert.throws(() => labels.saveRepositoryDisplayName(null, id, '项目'), /当前浏览器存储不可用/u)
  win.localStorage.setItem = () => { throw new Error('storage unavailable') }
  assert.throws(() => labels.saveRepositoryDisplayName(win, id, '项目'), /storage unavailable/u)
})
