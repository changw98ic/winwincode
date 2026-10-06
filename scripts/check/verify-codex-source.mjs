import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdtempSync, readFileSync, readdirSync, readlinkSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const root = resolve(import.meta.dirname, '../..')
const digest = bytes => createHash('sha256').update(bytes).digest('hex')

export function codexSourceDigest(directory) {
  const hash = createHash('sha256')
  function visit(relative = '') {
    for (const entry of readdirSync(join(directory, relative), { withFileTypes: true })
      .sort((left, right) => left.name.localeCompare(right.name, 'en'))) {
      if (['.git', 'target', 'node_modules'].includes(entry.name)) continue
      const path = relative ? `${relative}/${entry.name}` : entry.name
      if (entry.isDirectory()) visit(path)
      else if (entry.isFile()) hash.update(path).update('\0file\0')
        .update(readFileSync(join(directory, path))).update('\0')
      else if (entry.isSymbolicLink()) hash.update(path).update('\0link\0')
        .update(readlinkSync(join(directory, path))).update('\0')
      else assert.fail(`unsupported upstream source entry: ${path}`)
    }
  }
  visit()
  return hash.digest('hex')
}

export function verifyCodexSource(archive, repositoryRoot = root) {
  const manifest = JSON.parse(readFileSync(join(repositoryRoot, 'third_party/codex.UPSTREAM.json')))
  const lock = JSON.parse(readFileSync(join(repositoryRoot, 'upstream/sources.lock.json')))
  for (const field of ['repository', 'tag', 'version', 'commit', 'archiveSha256', 'sourceTreeSha256']) {
    assert.equal(manifest[field], lock.codex[field], field)
  }
  assert.equal(digest(readFileSync(archive)), manifest.archiveSha256, 'upstream archive digest')
  const patches = lock.patches.filter(patch => patch.file.startsWith('upstream/patches/codex/') && !patch.planned)
  assert.deepEqual(manifest.patchesApplied, patches.map(patch => patch.file))
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-codex-source-'))
  try {
    const unpack = spawnSync('tar', ['-xzf', resolve(archive), '--strip-components=1', '-C', directory], { encoding: 'utf8' })
    assert.equal(unpack.status, 0, unpack.stderr)
    assert.deepEqual(manifest.excludedSourceExtensions, ['.md'])
    function removeExcluded(path) {
      for (const entry of readdirSync(path, { withFileTypes: true })) {
        const file = join(path, entry.name)
        if (entry.isDirectory()) removeExcluded(file)
        else if (entry.name.toLowerCase().endsWith('.md')) rmSync(file)
      }
    }
    removeExcluded(directory)
    for (const patch of patches) {
      const bytes = readFileSync(join(repositoryRoot, patch.file))
      assert.equal(digest(bytes), patch.patchSha256, patch.file)
      const result = spawnSync('patch', ['--batch', '--forward', '--fuzz=0', `-p${patch.stripComponents ?? 1}`, '-d', directory], {
        input: bytes, encoding: 'utf8',
      })
      assert.equal(result.status, 0, `${patch.file}\n${result.stdout}\n${result.stderr}`)
    }
    assert.equal(codexSourceDigest(directory), manifest.sourceTreeSha256, 'archive and patches reproduce the pinned tree')
    assert.equal(codexSourceDigest(join(repositoryRoot, 'third_party/codex')), manifest.sourceTreeSha256, 'vendored source drift')
    return manifest.version
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  assert.equal(process.argv.length, 3, 'usage: node scripts/check/verify-codex-source.mjs ARCHIVE')
  process.stdout.write(`Codex ${verifyCodexSource(process.argv[2])} source and patches verified\n`)
}
