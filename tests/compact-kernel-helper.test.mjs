import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, truncateSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { prepareCompactKernelHelper } from '../scripts/compact-kernel-helper.mjs'

const limit = 64 * 1024 * 1024
function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'wwc-compact-helper-'))
  t.after(() => rmSync(root, { recursive: true, force: true }))
  for (const path of ['debug', 'release', 'bin']) mkdirSync(join(root, path))
  const helper = join(root, 'debug/winwincode-kernel-helper')
  writeFileSync(helper, 'development helper')
  const cargo = join(root, 'bin/cargo')
  writeFileSync(cargo, `#!${process.execPath}
const fs = require('node:fs');
const path = require('node:path');
const root = process.env.WWC_COMPACT_FIXTURE;
fs.writeFileSync(path.join(root, 'cargo-args.json'), JSON.stringify(process.argv.slice(2)));
const output = path.join(root, 'release/winwincode-kernel-helper');
fs.writeFileSync(output, 'compact product helper');
if (process.env.WWC_COMPACT_OVERSIZE) fs.truncateSync(output, ${limit + 1});
`, { mode: 0o755 })
  return { root, helper, options: {
    root, targetDirectory: root,
    environment: { ...process.env, PATH: join(root, 'bin'), WWC_COMPACT_FIXTURE: root },
  } }
}

test('a helper within the authenticated image limit needs no rebuild', t => {
  const { helper, options } = fixture(t)
  truncateSync(helper, limit)
  chmodSync(helper, 0o700)
  assert.equal(prepareCompactKernelHelper(options), helper)
  assert.equal(statSync(helper).size, limit)
  assert.equal(statSync(helper).mode & 0o777, 0o755)
})

test('an oversized development helper is replaced by the compact product build', t => {
  const { root, helper, options } = fixture(t)
  truncateSync(helper, limit + 1)
  assert.equal(prepareCompactKernelHelper({ ...options, offline: true }), helper)
  assert.deepEqual(JSON.parse(readFileSync(join(root, 'cargo-args.json'), 'utf8')),
    ['build', '--release', '-p', 'winwincode-kernel-helper', '--locked', '--offline'])
  assert.equal(readFileSync(helper, 'utf8'), 'compact product helper')
  assert.equal(statSync(helper).mode & 0o777, 0o755)
})

test('an oversized release build cannot replace the development helper', t => {
  const { helper, options } = fixture(t)
  truncateSync(helper, limit + 1)
  options.environment.WWC_COMPACT_OVERSIZE = '1'
  assert.throws(() => prepareCompactKernelHelper(options), /exceeds the authenticated image limit/u)
  assert.equal(statSync(helper).size, limit + 1)
})

test('workspace tests use the compact helper after Cargo runs its binary tests', t => {
  const { root, helper, options } = fixture(t)
  writeFileSync(join(root, 'bin/cargo'), `#!${process.execPath}
const fs = require('node:fs');
const path = require('node:path');
const root = process.env.CARGO_TARGET_DIR;
const args = process.argv.slice(2);
const helper = path.join(root, 'debug/winwincode-kernel-helper');
fs.appendFileSync(path.join(root, 'commands.jsonl'), JSON.stringify(args) + '\\n');
if (args.includes('--no-run') || (args[0] === 'test' && args.includes('-p') && args.includes('winwincode-kernel-helper'))) {
  fs.writeFileSync(helper, 'freshly linked development helper');
  fs.truncateSync(helper, ${limit + 1});
} else if (args.includes('--release')) {
  fs.writeFileSync(path.join(root, 'release/winwincode-kernel-helper'), 'compact product helper');
} else {
  if (args.includes('--workspace') && !args.includes('--exclude')) {
    fs.writeFileSync(helper, 'Cargo restored its development binary');
    fs.truncateSync(helper, ${limit + 1});
  }
  if (fs.readFileSync(helper, 'utf8') !== 'compact product helper') process.exit(2);
}
`, { mode: 0o755 })
  const result = spawnSync(process.execPath,
    [fileURLToPath(new URL('../scripts/test-rust-workspace.mjs', import.meta.url))],
    { env: { ...options.environment, CARGO_TARGET_DIR: root }, encoding: 'utf8' })
  assert.equal(result.status, 0, result.stderr)
  assert.equal(readFileSync(helper, 'utf8'), 'compact product helper')
  assert.deepEqual(readFileSync(join(root, 'commands.jsonl'), 'utf8').trim().split('\n').map(line => JSON.parse(line)), [
    ['test', '--all-features', '--locked', '--workspace', '--no-run'],
    ['test', '--all-features', '--locked', '-p', 'winwincode-kernel-helper'],
    ['build', '--release', '-p', 'winwincode-kernel-helper', '--locked'],
    ['test', '--all-features', '--locked', '--workspace', '--exclude', 'winwincode-kernel-helper'],
  ])
})
