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
const manifest = JSON.parse(readFileSync(join(root, 'third_party/codex.UPSTREAM.json'), 'utf8'))
const patches = sourceLock.patches.filter(({ file, planned }) => (
  file.startsWith('upstream/patches/codex/') && !planned
))
// Measured from the archive whose commit and SHA-256 are pinned below.
const originalHashes = {
  'codex-rs/core/src/session/turn.rs': '8c27407110003a384cc7a9f85985d83ff824378f1feb22e7e5c19d927d77ca1d',
  'codex-rs/core/src/session/turn_tests.rs': '6572c864f660c8a594afac024a921868c0397c7db80c74114176a87027a97429',
}
const digest = bytes => createHash('sha256').update(bytes).digest('hex')

function handoffSections(patch) {
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
  const input = handoffSections(patch)
  if (!input) return
  const result = spawnSync('patch', [
    '--batch',
    reverse ? '--reverse' : '--forward',
    '--fuzz=0',
    `--strip=${patch.stripComponents ?? 1}`,
    '--directory', directory,
  ], { input, encoding: 'utf8', env: { ...process.env, TMPDIR: tmpdir() } })
  assert.equal(result.error, undefined, patch.file)
  assert.equal(result.status, 0, `${patch.file}\n${result.stdout}\n${result.stderr}`)
}

test('declared Codex patches reproduce both handoff files from the pinned source', t => {
  assert.equal(sourceLock.codex.commit, 'd27764b82f7118f674371e6d6e76271d9d606edb')
  assert.equal(sourceLock.codex.archiveSha256, '5226394058e04c5404fe737b84fbede38ebf41d88a72eb8b27f3d1c6ea23373b')
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-codex-handoff-replay-'))
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

test('Codex manifest, ordered patches, digests and handoff targets agree', () => {
  assert.deepEqual(manifest.patchesApplied, patches.map(({ file }) => file))
  for (const field of ['repository', 'tag', 'version', 'commit', 'archiveSha256', 'license']) {
    assert.equal(manifest[field], sourceLock.codex[field], field)
  }
  for (const patch of patches) {
    if (patch.patchSha256) {
      assert.equal(digest(readFileSync(join(root, patch.file))), patch.patchSha256, patch.file)
    }
  }
  const handoff = patches.find(({ id }) => id === 'codex-winwincode-integration')
  assert.ok(handoff)
  for (const path of Object.keys(originalHashes)) assert.ok(handoff.targets.includes(path), path)
  assert.equal(patches.at(-1), handoff)
})
