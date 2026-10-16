// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { chmodSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import test from 'node:test'
import { collectRuntimeDiagnostics } from '../scripts/collect-runtime-diagnostics.mjs'
import { runSessions, selectedSessionFiles, sessionFiles, sessionGroups } from '../scripts/run-api-production-sessions.mjs'
import { produceRuntimeProducts, restoreRuntimeProducts, runtimeProductFiles } from '../scripts/runtime-products.mjs'
import { writeApiProductionSourceSeal } from '../scripts/run-api-production-vertical.mjs'

test('four serial session groups cover exactly the original fourteen files once', () => {
  const original = ['api-production-vertical', 'chat-jev-recovery-production',
    'chat-execution-recovery-production', 'chat-execution-load-production',
    'chat-execution-auxiliary-production', 'chat-execution-dispatch-prepare-production',
    'chat-execution-ack-loss-production', 'chat-execution-transport-production',
    'chat-execution-obsolete-owner-production', 'chat-call-pairs-production',
    'chat-execution-tool-boundary-production', 'chat-execution-bounded-replay-production',
    'chat-execution-error-boundaries-production', 'strongflow-fusion-production']
    .map(name => `tests/${name}.test.mjs`)
  assert.deepEqual(sessionFiles, original)
  assert.deepEqual(selectedSessionFiles(), original)
  const grouped = Object.keys(sessionGroups).flatMap(selectedSessionFiles)
  assert.equal(Object.keys(sessionGroups).length, 4)
  assert.equal(new Set(grouped).size, 14)
  assert.deepEqual(grouped.toSorted(), original.toSorted())
  for (const path of grouped) assert.ok(existsSync(new URL(`../${path}`, import.meta.url)))
  assert.throws(() => selectedSessionFiles('typo'), /unknown production session group/u)
})

test('original source seal relocates with executable products and a consumer-only rerun needs no compiler', t => {
  const base = mkdtempSync(join(tmpdir(), 'wwc-runtime-products-'))
  t.after(() => rmSync(base, { recursive: true, force: true }))
  const root = join(base, 'producer')
  function file(path, content, mode = 0o644) {
    mkdirSync(join(root, path, '..'), { recursive: true })
    writeFileSync(join(root, path), content, { mode })
    chmodSync(join(root, path), mode)
  }
  file('package.json', '{"version":"0.1.0"}\n')
  file('Cargo.toml', '[workspace.package]\nversion="0.1.0"\n')
  file('.gitignore', 'target/\n')
  file('crates/helper/src/main.rs', 'fn main() {}\n')
  for (const name of ['run-api-production-vertical', 'product-build-contract', 'build-community', 'device-production-fixture']) {
    file(`scripts/${name}.mjs`, 'export {}\n')
  }
  for (const path of sessionFiles) file(path, `import assert from 'node:assert/strict'\nimport { execFileSync } from 'node:child_process'\nimport { appendFileSync } from 'node:fs'\nimport test from 'node:test'\ntest('synthetic relocated consumer', () => {\nassert.equal(process.env.WWC_API_SKIP_BUILD, '1')\nassert.equal(execFileSync(process.env.WWC_CLI_BINARY, { encoding: 'utf8' }), 'fixture-product\\n')\nassert.equal(process.env.GITHUB_RUN_ATTEMPT, '2')\nappendFileSync(process.env.FIXTURE_REPORT, import.meta.url + '\\n')\n})\n`)
  for (const name of runtimeProductFiles.slice(0, 4)) file(`target/debug/${name}`, '#!/bin/sh\nprintf "fixture-product\\n"\n', 0o755)
  const git = (...args) => execFileSync('git', ['-C', root, ...args], { encoding: 'utf8' })
  git('init', '-q')
  git('add', '.')
  git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture')
  const sealed = writeApiProductionSourceSeal({ root,
    serverBinary: join(root, 'target/debug/winwincode-server'),
    helperExecutable: join(root, 'target/debug/winwincode-kernel-helper') })
  chmodSync(sealed.path, 0o644)
  const originalSeal = readFileSync(sealed.path)
  const output = join(base, 'archive')
  const env = { ...process.env, GITHUB_RUN_ID: '12345', GITHUB_RUN_ATTEMPT: '1', GITHUB_OUTPUT: join(base, 'outputs') }
  produceRuntimeProducts(root, output, env)
  const manifest = JSON.parse(readFileSync(join(output, 'manifest.json')))
  assert.ok(readFileSync(env.GITHUB_OUTPUT, 'utf8').includes(`archive_sha256=${manifest.sha256}\nproducer_attempt=1\n`))
  const moved = join(base, 'consumer')
  cpSync(root, moved, { recursive: true, filter: path => path !== join(root, 'target') })
  renameSync(root, join(base, 'old-producer-unavailable'))
  const tools = join(base, 'no-compilers')
  mkdirSync(tools)
  for (const name of ['cargo', 'rustc']) writeFileSync(join(tools, name), '#!/bin/sh\nexit 91\n', { mode: 0o755 })
  const consumer = { ...env, GITHUB_RUN_ATTEMPT: '2', WWC_RUNTIME_PRODUCER_ATTEMPT: '1',
    WWC_RUNTIME_ARCHIVE_SHA256: manifest.sha256, PATH: `${tools}:${env.PATH}` }
  delete consumer.NODE_TEST_CONTEXT
  consumer.FIXTURE_REPORT = join(base, 'executed')
  restoreRuntimeProducts(moved, output, consumer)
  assert.deepEqual(readFileSync(join(moved, 'target/debug/winwincode-api-production.source.json')), originalSeal)
  for (const name of runtimeProductFiles.slice(0, 4)) assert.equal(statSync(join(moved, 'target/debug', name)).mode & 0o777, 0o755)
  for (const group of Object.keys(sessionGroups)) assert.equal(runSessions(moved, group, consumer), 0)
  assert.equal(new Set(readFileSync(consumer.FIXTURE_REPORT, 'utf8').trim().split('\n')).size, 14)
  for (const [override, message] of [
    [{ WWC_RUNTIME_PRODUCER_ATTEMPT: '2' }, /attempt mismatch/u],
    [{ WWC_RUNTIME_PRODUCER_ATTEMPT: '' }, /attempt output is required/u],
    [{ WWC_RUNTIME_ARCHIVE_SHA256: '0'.repeat(64) }, /producer digest mismatch/u],
    [{ GITHUB_RUN_ID: '999' }, /run mismatch/u],
  ]) assert.throws(() => restoreRuntimeProducts(moved, output, { ...consumer, ...override }), message)
  chmodSync(join(moved, 'target/debug/wwc'), 0o644)
  assert.throws(() => runSessions(moved, 'load', consumer), /CLI binary mode changed/u)
  restoreRuntimeProducts(moved, output, consumer)
  writeFileSync(join(moved, 'target/debug/winwincode-worker'), 'changed')
  assert.throws(() => runSessions(moved, 'load', consumer), /Worker binary digest changed/u)
  writeFileSync(join(output, 'products.tar'), 'corrupt')
  assert.throws(() => restoreRuntimeProducts(moved, output, consumer), /archive checksum mismatch/u)
  manifest.commit = '0'.repeat(40)
  writeFileSync(join(output, 'manifest.json'), JSON.stringify(manifest))
  assert.throws(() => restoreRuntimeProducts(moved, output, consumer), /commit mismatch/u)
})

test('failed scenario diagnostics retain safe logs and database counts without credentials or database payloads', t => {
  const temporary = mkdtempSync(join(tmpdir(), 'wwc-runtime-diagnostics-'))
  t.after(() => rmSync(temporary, { recursive: true, force: true }))
  const output = join(temporary, 'output')
  const transcript = join(temporary, 'sessions.log')
  writeFileSync(transcript, 'fixture scenario failed at its original attempt\n')
  writeFileSync(join(temporary, 'device-process.log'), 'Authorization: Bearer synthetic-private-credential\n')
  writeFileSync(join(temporary, 'private-key.pem'), 'unrelated secret')
  const database = new DatabaseSync(join(temporary, 'worker.sqlite3'))
  database.exec("CREATE TABLE execution_outbox (payload TEXT); INSERT INTO execution_outbox VALUES ('synthetic-private-payload'); CREATE TABLE credentials (secret TEXT)")
  database.close()
  collectRuntimeDiagnostics(temporary, transcript, output)
  const report = readFileSync(join(output, 'diagnostics.json'), 'utf8')
  assert.equal(JSON.parse(report).databases[0].tables.execution_outbox, 1)
  assert.equal(readFileSync(join(output, 'scenario-0.log'), 'utf8'), readFileSync(transcript, 'utf8'))
  assert.equal(existsSync(join(output, 'scenario-1.log')), false)
  assert.ok(!report.includes('synthetic-private'))
  assert.ok(!report.includes('credentials'))
  assert.equal(existsSync(join(output, 'worker.sqlite3')), false)
})
