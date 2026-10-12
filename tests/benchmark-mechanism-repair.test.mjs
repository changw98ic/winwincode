// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFile } from 'node:child_process'
import { mkdtemp, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'
import test from 'node:test'
import { promisify } from 'node:util'

test('benchmark interruption reconciliation and admission state retain exact original execution', async t => {
  const directory = await mkdtemp(resolve(tmpdir(), 'benchmark-mechanism-repair-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const root = resolve(import.meta.dirname, '..')
  await promisify(execFile)(process.execPath, [resolve(root, 'tests/mechanism/scheduler-regression.mjs'),
    root, directory], { timeout: 90_000 })
  const receipt = JSON.parse(await readFile(resolve(directory, 'scheduler-js-receipt.json'), 'utf8'))
  assert.deepEqual(receipt.cases.map(({ id, regression, status }) => ({ id, regression, status })), [
    { id: 'M18', regression: 'green', status: undefined },
    { id: 'M19', regression: 'green', status: undefined },
    { id: 'M21', regression: undefined, status: 'confirmed' },
  ])
})
