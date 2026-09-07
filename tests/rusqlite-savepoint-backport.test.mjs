import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import {
  cpSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, relative, resolve } from 'node:path'
import { spawnSync } from 'node:child_process'
import test from 'node:test'
import { releaseSourcePaths } from '../scripts/release-source-contract.mjs'

const root = resolve(import.meta.dirname, '..')
const sourceLock = JSON.parse(readFileSync(join(root, 'upstream', 'sources.lock.json'), 'utf8'))
const vendoredSource = sourceLock.vendoredCargoSources.find(({ package: packageName }) => (
  packageName === 'rusqlite'
))
assert.ok(vendoredSource, 'rusqlite source identity must be recorded')
const vendorRoot = join(root, vendoredSource.sourceDirectory)
const patchPath = join(root, vendoredSource.patch)

function sha256(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex')
}

function sourceTreeSha256(directory) {
  const files = []
  const visit = current => {
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      const path = join(current, entry.name)
      if (entry.isDirectory()) visit(path)
      else if (entry.isFile() && entry.name !== 'Cargo.lock') files.push(relative(directory, path).replaceAll('\\', '/'))
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

function lockPackages(lock, packageName) {
  return lock
    .split('[[package]]')
    .filter(section => new RegExp(`^\\s*name = "${packageName}"$`, 'mu').test(section))
}

test('Cargo selects one patched rusqlite source and one SQLite native library', () => {
  const manifest = readFileSync(join(root, 'Cargo.toml'), 'utf8')
  assert.match(
    manifest,
    /^rusqlite = \{ path = "upstream\/vendor\/rusqlite-0\.39\.0" \}$/mu,
  )

  const lock = readFileSync(join(root, 'Cargo.lock'), 'utf8')
  const rusqlitePackages = lockPackages(lock, 'rusqlite')
  assert.equal(rusqlitePackages.length, 1)
  assert.match(rusqlitePackages[0], /^version = "0\.39\.0"$/mu)
  assert.doesNotMatch(rusqlitePackages[0], /^source = /mu)
  assert.doesNotMatch(rusqlitePackages[0], /^checksum = /mu)

  const sqlitePackages = lockPackages(lock, 'libsqlite3-sys')
  assert.equal(sqlitePackages.length, 1)
  assert.match(sqlitePackages[0], /^version = "0\.37\.0"$/mu)
})

test('vendored source identity, patch and MIT license are exact', () => {
  assert.deepEqual(
    {
      package: vendoredSource.package,
      version: vendoredSource.version,
      registryChecksumSha256: vendoredSource.registryChecksumSha256,
      upstreamCommit: vendoredSource.upstreamCommit,
      license: vendoredSource.license,
      fixUpstreamCommit: vendoredSource.fixUpstreamCommit,
      fixReleasedIn: vendoredSource.fixReleasedIn,
    },
    {
      package: 'rusqlite',
      version: '0.39.0',
      registryChecksumSha256: 'a0d2b0146dd9661bf67bb107c0bb2a55064d556eeb3fc314151b957f313bcd4e',
      upstreamCommit: '2a1790a69107cd03dae85d501dcbdb11c5b32ef3',
      license: 'MIT',
      fixUpstreamCommit: '15385cc046364b68c9d7e65d2644dc86c0980f25',
      fixReleasedIn: 'rusqlite 0.40.1',
    },
  )
  assert.equal(sourceTreeSha256(vendorRoot), vendoredSource.patchedSourceTreeSha256)
  assert.equal(
    sha256(join(vendorRoot, vendoredSource.upstreamSourceFile)),
    vendoredSource.patchedSourceFileSha256,
  )
  assert.equal(sha256(patchPath), vendoredSource.patchSha256)
  assert.equal(sha256(join(root, vendoredSource.licenseFile)), vendoredSource.licenseFileSha256)

  const cargoManifest = readFileSync(join(vendorRoot, 'Cargo.toml'), 'utf8')
  assert.match(cargoManifest, /^name = "rusqlite"$/mu)
  assert.match(cargoManifest, /^version = "0\.39\.0"$/mu)
  assert.match(cargoManifest, /^license = "MIT"$/mu)

  const patchRecord = sourceLock.patches.find(({ id }) => (
    id === 'rusqlite-quote-savepoint-identifiers'
  ))
  assert.deepEqual(patchRecord, {
    id: 'rusqlite-quote-savepoint-identifiers',
    file: vendoredSource.patch,
    planned: false,
    targets: ['upstream/vendor/rusqlite-0.39.0/src/transaction.rs'],
  })

  const notices = readFileSync(join(root, 'THIRD_PARTY_NOTICES.md'), 'utf8')
  assert.match(notices, /## rusqlite/u)
  assert.match(notices, /Copyright \(c\) 2014 The rusqlite developers/u)
  assert.match(notices, /upstream\/vendor\/rusqlite-0\.39\.0\/LICENSE/u)
})

test('the exact patch reverses to the recorded crates.io source', t => {
  const temporaryRoot = mkdtempSync(join(tmpdir(), 'winwincode-rusqlite-'))
  t.after(() => rmSync(temporaryRoot, { force: true, recursive: true }))
  const cleanSource = join(temporaryRoot, 'rusqlite-0.39.0')
  cpSync(vendorRoot, cleanSource, { recursive: true })

  const result = spawnSync('patch', [
    '--batch',
    '--reverse',
    '--strip=1',
    '--directory', cleanSource,
    '--input', patchPath,
  ], { encoding: 'utf8' })
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`)
  assert.equal(sourceTreeSha256(cleanSource), vendoredSource.upstreamSourceTreeSha256)
  assert.equal(
    sha256(join(cleanSource, vendoredSource.upstreamSourceFile)),
    vendoredSource.upstreamSourceFileSha256,
  )
})

test('named savepoint commands quote the caller-provided identifier', () => {
  const source = readFileSync(join(vendorRoot, 'src', 'transaction.rs'), 'utf8')
  assert.match(source, /use crate::pragma::Sql;/u)
  assert.match(source, /fn cmd\(cmd: &'static str, to: bool, name: &str\) -> Result<Sql>/u)
  assert.match(source, /sql\.push_identifier\(name\);/u)
  assert.match(source, /cmd\("SAVEPOINT", false, name\.as_str\(\)\)\?/u)
  assert.match(source, /cmd\("RELEASE", false, self\.name\.as_str\(\)\)\?/u)
  assert.match(source, /cmd\("ROLLBACK", true, self\.name\.as_str\(\)\)\?/u)
  assert.doesNotMatch(source, /format!\("SAVEPOINT \{name\}"\)/u)
  assert.doesNotMatch(source, /format!\("RELEASE \{\}", self\.name\)/u)
  assert.doesNotMatch(source, /format!\("ROLLBACK TO \{\}", self\.name\)/u)
})

test('release source inventory includes the vendored source, patch and license', () => {
  const paths = new Set(releaseSourcePaths(root))
  for (const path of [
    'upstream/patches/rusqlite/0001-quote-savepoint-identifiers.patch',
    'upstream/vendor/rusqlite-0.39.0/Cargo.toml',
    'upstream/vendor/rusqlite-0.39.0/LICENSE',
    'upstream/vendor/rusqlite-0.39.0/src/transaction.rs',
  ]) {
    assert.equal(paths.has(path), true, `${path} must be a release source input`)
  }
})
