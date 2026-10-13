import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const sourceLock = JSON.parse(readFileSync(join(root, 'upstream/sources.lock.json'), 'utf8'))
const patches = sourceLock.patches.filter(({ file, planned }) => (
  file.startsWith('upstream/patches/codex/') && !planned
))
// Measured from the verified 758ef40 archive, not from reversing the local patches.
const originalHashes = {
  'codex-rs/codex-api/src/api_bridge.rs': 'a6c185eac08d05f7d28e58bb88fde713db10a14bcf48dcf37a84db4be1bdfd99',
  'codex-rs/codex-api/src/error.rs': '3fd2ea3cd5490b1f744e98d51a283fbe09aa940bfbd7eb9bacf2f3fe9c512dd0',
  'codex-rs/protocol/src/error.rs': '982d6968960b97ccb592c31c7b1cea4832b541be1f22ac054fa15c2b5b2b2d10',
  'codex-rs/response-debug-context/src/lib.rs': '24fd5dc1d01baf481d53a45a64e5ef149e5db909fc51d8303da7cccafdba19bd',
  'codex-rs/cli/src/doctor.rs': '32497fa13ca6c713cf71d66f75043bed1d03ed891d0d526012f96c1f984d42d5',
  'codex-rs/ext/guardian-v2/src/async_scorer/sampler.rs': 'b3fec0b9d3ed6acd14d24a5a51ca12e42607ab48f1dd62a652c3cb3379136648',
}
const digest = bytes => createHash('sha256').update(bytes).digest('hex')

function errorSections(patch) {
  const strip = patch.stripComponents ?? 1
  assert.equal(Number.isInteger(strip) && strip >= 1, true, patch.file)
  return readFileSync(join(root, patch.file), 'utf8')
    .split(/(?=^diff --git )/mu)
    .filter(section => {
      const path = section.match(/^\+\+\+ (\S+)$/mu)?.[1]
        ?.split('/').slice(strip).join('/')
      if (!Object.hasOwn(originalHashes, path ?? '')) return false
      assert.equal(patch.targets.includes(path), true, `${patch.file}: undeclared ${path}`)
      return true
    })
    .join('')
}

function apply(directory, patch, reverse = false) {
  const input = errorSections(patch)
  if (!input) return
  const result = spawnSync('patch', [
    '--batch', reverse ? '--reverse' : '--forward', '--fuzz=0',
    `--strip=${patch.stripComponents ?? 1}`, '--directory', directory,
  ], { input, encoding: 'utf8', env: { ...process.env, TMPDIR: tmpdir() } })
  assert.equal(result.error, undefined, patch.file)
  assert.equal(result.status, 0, `${patch.file}\n${result.stdout}\n${result.stderr}`)
}

test('host model error patches replay exactly from every pinned original source file', t => {
  assert.equal(sourceLock.codex.commit, '758ef40f50c1a458425c7cfbf1eb12cbc07af0b0')
  assert.equal(sourceLock.codex.archiveSha256, '0413a0e7680bcc2b6c6e998a6ad358115707317ef5d0121dcb9275e88c36121a')
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-host-model-replay-'))
  t.after(() => rmSync(directory, { force: true, recursive: true }))
  const current = new Map()
  for (const path of Object.keys(originalHashes)) {
    const bytes = readFileSync(join(root, 'third_party/codex', path))
    current.set(path, bytes)
    mkdirSync(dirname(join(directory, path)), { recursive: true })
    writeFileSync(join(directory, path), bytes)
  }
  for (const patch of [...patches].reverse()) apply(directory, patch, true)
  for (const [path, expected] of Object.entries(originalHashes)) {
    assert.equal(digest(readFileSync(join(directory, path))), expected, `${path}: unrecorded source change`)
  }
  for (const patch of patches) apply(directory, patch)
  for (const [path, bytes] of current) {
    assert.deepEqual(readFileSync(join(directory, path)), bytes, `${path}: replay differs`)
  }
})

test('host model retry patch has declared source targets and a verified digest', () => {
  const patch = patches.find(({ id }) => id === 'codex-host-model-retry-semantics')
  assert.ok(patch)
  assert.deepEqual([...patch.targets].sort(), Object.keys(originalHashes).sort())
  assert.equal(digest(readFileSync(join(root, patch.file))), patch.patchSha256)
  assert.deepEqual(patches.filter(({ targets }) => targets.some(path => Object.hasOwn(originalHashes, path))).map(({ id }) => id),
    ['codex-host-model-retry-semantics', 'codex-execution-terminal-cause-and-drain', 'codex-model-authority-expiry-cause'])
})

test('internal error schema binary patch reproduces the pinned artifact exactly', t => {
  const path = 'codex-rs/app-server-protocol/schema/precomputed/app-server-exports-stable.json.zst'
  const originalHash = '5b0cb2524f4719764b11fe4a0321ca300a61daf081bbc391a21e2417254dafb3'
  const patch = patches.find(({ id }) => id === 'codex-app-server-internal-error-schema')
  assert.ok(patch)
  assert.deepEqual(patch.targets, [path])
  assert.equal(digest(readFileSync(join(root, patch.file))), patch.patchSha256)
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-internal-schema-replay-'))
  t.after(() => rmSync(directory, { force: true, recursive: true }))
  const current = readFileSync(join(root, 'third_party/codex', path))
  mkdirSync(dirname(join(directory, path)), { recursive: true })
  writeFileSync(join(directory, path), current)
  for (const reverse of [true, false]) {
    const result = spawnSync('git', [
      'apply', '--binary', ...(reverse ? ['--reverse'] : []), join(root, patch.file),
    ], { cwd: directory, encoding: 'utf8' })
    assert.equal(result.error, undefined, patch.file)
    assert.equal(result.status, 0, `${patch.file}\n${result.stdout}\n${result.stderr}`)
    if (reverse) assert.equal(digest(readFileSync(join(directory, path))), originalHash)
    else assert.deepEqual(readFileSync(join(directory, path)), current)
  }
})
