import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { cpSync, mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, relative, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const sourceLock = JSON.parse(readFileSync(join(root, 'upstream', 'sources.lock.json'), 'utf8'))
const source = sourceLock.vendoredCargoSources.find(item => item.package === 'rusqlite')
assert.notEqual(source, undefined, 'rusqlite source identity must be recorded')
const vendorRoot = join(root, source.sourceDirectory)
const patchPath = join(root, source.patch)

function sha256(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex')
}

function sourceTreeSha256(directory) {
  const files = []
  const visit = current => {
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      const path = join(current, entry.name)
      if (entry.isDirectory()) visit(path)
      else if (entry.isFile() && entry.name !== 'Cargo.lock') {
        files.push(relative(directory, path).replaceAll('\\', '/'))
      }
    }
  }
  visit(directory)
  const hash = createHash('sha256')
  for (const path of files.sort()) {
    hash.update(path)
    hash.update('\0')
    hash.update(readFileSync(join(directory, path)))
    hash.update('\0')
  }
  return hash.digest('hex')
}

test('Cargo selects the single patched rusqlite 0.39.0 source', () => {
  const manifest = readFileSync(join(root, 'Cargo.toml'), 'utf8')
  assert.match(manifest, /^rusqlite = \{ path = "upstream\/vendor\/rusqlite-0\.39\.0" \}$/mu)
  const lock = readFileSync(join(root, 'Cargo.lock'), 'utf8')
  const packages = lock.split('[[package]]').filter(section => /^\s*name = "rusqlite"$/mu.test(section))
  assert.equal(packages.length, 1)
  assert.match(packages[0], /^version = "0\.39\.0"$/mu)
  assert.doesNotMatch(packages[0], /^source = /mu)
  assert.doesNotMatch(packages[0], /^checksum = /mu)
  const sqlitePackages = lock.split('[[package]]').filter(section =>
    /^\s*name = "libsqlite3-sys"$/mu.test(section),
  )
  assert.equal(sqlitePackages.length, 1)
  assert.match(sqlitePackages[0], /^version = "0\.37\.0"$/mu)
})

test('vendored rusqlite source, patch, and license hashes are reproducible', () => {
  assert.equal(source.version, '0.39.0')
  assert.equal(source.registryChecksumSha256, 'a0d2b0146dd9661bf67bb107c0bb2a55064d556eeb3fc314151b957f313bcd4e')
  assert.equal(sourceTreeSha256(vendorRoot), source.patchedSourceTreeSha256)
  assert.equal(sha256(join(vendorRoot, source.upstreamSourceFile)), source.patchedSourceFileSha256)
  assert.equal(sha256(patchPath), source.patchSha256)
  assert.equal(sha256(join(root, source.licenseFile)), source.licenseFileSha256)
  assert.equal(source.license, 'MIT')
})

test('the exact patch reverses to the recorded crates.io source', t => {
  const temporaryRoot = mkdtempSync(join(tmpdir(), 'winwincode-rusqlite-'))
  t.after(() => rmSync(temporaryRoot, { force: true, recursive: true }))
  const cleanSource = join(temporaryRoot, 'rusqlite-0.39.0')
  cpSync(vendorRoot, cleanSource, { recursive: true })
  const result = spawnSync('patch', [
    '--batch', '--reverse', '--strip=1', '--directory', cleanSource, '--input', patchPath,
  ], { encoding: 'utf8' })
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`)
  assert.equal(sourceTreeSha256(cleanSource), source.upstreamSourceTreeSha256)
  assert.equal(sha256(join(cleanSource, source.upstreamSourceFile)), source.upstreamSourceFileSha256)
})
