import assert from 'node:assert/strict'
import test from 'node:test'

import {
  parsePnpmPackReport,
  pnpmPackDryRun,
} from '../scripts/pnpm-pack-report.mjs'

test('package inspection uses the pinned pnpm pack report', () => {
  const calls = []
  const files = pnpmPackDryRun('/workspace/package', (command, arguments_, options) => {
    calls.push({ command, arguments_, options })
    return {
      error: undefined,
      status: 0,
      stderr: '',
      stdout: JSON.stringify({
        name: '@winwincode/native-linux-x64',
        files: [
          { path: 'package.json', size: 100 },
          { path: 'prebuild/LICENSE', size: 200 },
        ],
      }),
    }
  })

  assert.deepEqual(calls, [{
    command: 'corepack',
    arguments_: ['pnpm', 'pack', '--dry-run', '--json'],
    options: {
      cwd: '/workspace/package',
      encoding: 'utf8',
    },
  }])
  assert.deepEqual(files, ['package.json', 'prebuild/LICENSE'])
})

test('package inspection rejects an array report', () => {
  assert.throws(
    () => parsePnpmPackReport(JSON.stringify([{ files: [{ path: 'package.json' }] }])),
    /pnpm pack report must be an object/u,
  )
})

test('package inspection reports a failed pnpm pack command', () => {
  assert.throws(
    () => pnpmPackDryRun('/workspace/package', () => ({
      error: undefined,
      status: 1,
      stderr: 'pack failed',
      stdout: '',
    })),
    /pnpm pack failed: pack failed/u,
  )
})
