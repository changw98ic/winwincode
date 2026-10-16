// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync, realpathSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { serverTargetDirectory, verifyApiProductionSourceSeal } from './run-api-production-vertical.mjs'

export const sessionFiles = Object.freeze([
  'tests/api-production-vertical.test.mjs',
  'tests/chat-jev-recovery-production.test.mjs',
  'tests/chat-execution-recovery-production.test.mjs',
  'tests/chat-execution-load-production.test.mjs',
  'tests/chat-execution-auxiliary-production.test.mjs',
  'tests/chat-execution-dispatch-prepare-production.test.mjs',
  'tests/chat-execution-ack-loss-production.test.mjs',
  'tests/chat-execution-transport-production.test.mjs',
  'tests/chat-execution-obsolete-owner-production.test.mjs',
  'tests/chat-call-pairs-production.test.mjs',
  'tests/chat-execution-tool-boundary-production.test.mjs',
  'tests/chat-execution-bounded-replay-production.test.mjs',
  'tests/chat-execution-error-boundaries-production.test.mjs',
  'tests/strongflow-fusion-production.test.mjs',
])

export const sessionGroups = Object.freeze({
  load: ['chat-execution-load'],
  api: ['api-production-vertical', 'chat-jev-recovery', 'chat-execution-dispatch-prepare'],
  recovery: ['chat-execution-recovery', 'chat-execution-obsolete-owner', 'chat-call-pairs',
    'chat-execution-ack-loss', 'chat-execution-auxiliary'],
  boundaries: ['chat-execution-error-boundaries', 'chat-execution-bounded-replay',
    'chat-execution-transport', 'chat-execution-tool-boundary', 'strongflow-fusion'],
})

export function selectedSessionFiles(group) {
  if (group === undefined) return [...sessionFiles]
  assert.ok(Object.hasOwn(sessionGroups, group), `unknown production session group: ${group}`)
  return sessionGroups[group].map(name => `tests/${name}${name === 'api-production-vertical' ? '' : '-production'}.test.mjs`)
}

export function runSessions(root, group, env = process.env) {
  const files = selectedSessionFiles(group)
  const canonicalDirectory = join(serverTargetDirectory(root), 'debug')
  const directory = env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR ?? canonicalDirectory
  assert.equal(realpathSync(directory), realpathSync(canonicalDirectory),
    'all production sessions require the configured canonical target directory')
  const binaries = { root, serverBinary: join(directory, 'winwincode-server'),
    helperExecutable: join(directory, 'winwincode-kernel-helper') }
  const verified = verifyApiProductionSourceSeal(binaries)
  const seal = readFileSync(verified.path)
  const result = spawnSync(process.execPath, [
    '--test', '--test-concurrency=1', ...files,
  ], {
    cwd: root,
    stdio: 'inherit',
    env: { ...env, WWC_API_SKIP_BUILD: '1',
      WWC_CODE_MODE_ACCEPTANCE_BIN_DIR: directory, WWC_CLI_BINARY: join(directory, 'wwc') },
  })
  verifyApiProductionSourceSeal(binaries)
  assert.deepEqual(readFileSync(verified.path), seal,
    'production sessions must retain the original verified source seal')
  if (result.error) throw result.error
  return result.status ?? 1
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  const args = process.argv.slice(2)
  assert.ok(args.length === 0 || (args.length === 2 && args[0] === '--group'),
    'usage: run-api-production-sessions.mjs [--group load|api|recovery|boundaries]')
  process.exitCode = runSessions(resolve(import.meta.dirname, '..'), args[1])
}
