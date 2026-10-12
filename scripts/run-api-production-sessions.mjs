// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync, realpathSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { serverTargetDirectory, verifyApiProductionSourceSeal } from './run-api-production-vertical.mjs'

const root = fileURLToPath(new URL('../', import.meta.url))
const canonicalDirectory = join(serverTargetDirectory(root), 'debug')
const directory = process.env.WWC_CODE_MODE_ACCEPTANCE_BIN_DIR ?? canonicalDirectory
assert.equal(realpathSync(directory), realpathSync(canonicalDirectory),
  'all production sessions require the configured canonical target directory')
const binaries = { root, serverBinary: join(directory, 'winwincode-server'),
  helperExecutable: join(directory, 'winwincode-kernel-helper') }
const verified = verifyApiProductionSourceSeal(binaries)
const seal = readFileSync(verified.path)
const result = spawnSync(process.execPath, [
  '--test', '--test-concurrency=1',
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
], {
  cwd: root,
  stdio: 'inherit',
  env: { ...process.env, WWC_API_SKIP_BUILD: '1',
    WWC_CODE_MODE_ACCEPTANCE_BIN_DIR: directory, WWC_CLI_BINARY: join(directory, 'wwc') },
})
verifyApiProductionSourceSeal(binaries)
assert.deepEqual(readFileSync(verified.path), seal,
  'production sessions must retain the original verified source seal')
if (result.error) throw result.error
process.exitCode = result.status ?? 1
