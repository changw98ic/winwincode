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
// Baseline files from the pinned Codex tree, before the Code Mode definition patch.
const originalHashes = {
  "codex-rs/code-mode-protocol/src/description.rs": "a85090f7b639db03dc336937490b3230e37203da5d780d5c20b0bf378276ee24",
  "codex-rs/code-mode-runtime/src/cell_actor/conversions.rs": "6030fa4322e4c198c296f8f1b9190f02185411d7dfdb056cde2cd9da9b4c2237",
  "codex-rs/code-mode-runtime/src/runtime/callbacks.rs": "79d2bf693103a238dfc5f7aee4f6b216955e9f01a4da4a90aebc32884a0f428c",
  "codex-rs/code-mode-runtime/src/runtime/globals.rs": "e62df37ddd8d36f3f799d3a2713b225661e3706dcaf2fe97b3cf183427903c07",
  "codex-rs/code-mode-runtime/src/service.rs": "24e7bc94f6f9318f0612f6f37fdf5ffbacfccd454a2de43c98fdc353aad1c018",
  "codex-rs/code-mode-runtime/src/service_tests.rs": "1145cbeb4689592af348957265052a7b1347c44b9cfa1c5473858f12b8cb5f4b",
  "codex-rs/code-mode-runtime/src/session_runtime/types.rs": "30b92086f42d5eee5b5e1d053b07563219d8dcd18fecad9e38c663c1ad194fe2",
  "codex-rs/core/src/tools/code_mode/execute_handler.rs": "3833a098b4b7f8506b18a65a943818ba7933cccce1765ffd1b5fecefdde0a9cc",
  "codex-rs/core/src/tools/handlers/mcp.rs": "b2b6a2c138d576e7b3818240c83f875728d59366eb858403b79097bd2b9b0db1"
}
const digest = bytes => createHash('sha256').update(bytes).digest('hex')

function definitionSections(patch) {
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
  const input = definitionSections(patch)
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

test('declared Codex patches reproduce the Code Mode definition files from the pinned source', t => {
  assert.equal(sourceLock.codex.commit, '758ef40f50c1a458425c7cfbf1eb12cbc07af0b0')
  assert.equal(sourceLock.codex.archiveSha256, '0413a0e7680bcc2b6c6e998a6ad358115707317ef5d0121dcb9275e88c36121a')
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-code-mode-replay-'))
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

test('Code Mode definition patch retains its target and digest identity', () => {
  assert.deepEqual(manifest.patchesApplied, patches.map(({ file }) => file))
  for (const field of ['repository', 'tag', 'version', 'commit', 'archiveSha256', 'license']) {
    assert.equal(manifest[field], sourceLock.codex[field], field)
  }
  for (const patch of patches) {
    if (patch.patchSha256) {
      assert.equal(digest(readFileSync(join(root, patch.file))), patch.patchSha256, patch.file)
    }
  }
  const definitions = patches.find(({ id }) => id === 'codex-code-mode-authorized-tool-definitions')
  assert.ok(definitions)
  assert.deepEqual(definitions.targets, Object.keys(originalHashes))
  assert.equal(patches.indexOf(definitions), patches.findIndex(({ id }) => id === 'codex-record-delegated-handoff-output') + 1)
})
