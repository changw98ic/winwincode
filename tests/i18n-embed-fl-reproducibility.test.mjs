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
const [vendoredSource] = sourceLock.vendoredCargoSources
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
      else if (entry.isFile()) files.push(relative(directory, path).replaceAll('\\', '/'))
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

test('Cargo selects one patched i18n-embed-fl 0.9.4 source', () => {
  const manifest = readFileSync(join(root, 'Cargo.toml'), 'utf8')
  assert.match(
    manifest,
    /^i18n-embed-fl = \{ path = "upstream\/vendor\/i18n-embed-fl-0\.9\.4" \}$/mu,
  )

  const lock = readFileSync(join(root, 'Cargo.lock'), 'utf8')
  const packages = lock
    .split('[[package]]')
    .filter(section => /^\s*name = "i18n-embed-fl"$/mu.test(section))
  assert.equal(packages.length, 1)
  assert.match(packages[0], /^version = "0\.9\.4"$/mu)
  assert.doesNotMatch(packages[0], /^source = /mu)
  assert.doesNotMatch(packages[0], /^checksum = /mu)
})

test('vendored source identity, patch and MIT license are exact', () => {
  assert.equal(sourceLock.vendoredCargoSources.length, 1)
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
      package: 'i18n-embed-fl',
      version: '0.9.4',
      registryChecksumSha256: '04b2969d0b3fc6143776c535184c19722032b43e6a642d710fa3f88faec53c2d',
      upstreamCommit: 'ceb3da0ee3acf91b17a7a52e02642267ddb47a3d',
      license: 'MIT',
      fixUpstreamCommit: 'f02d3ca8acb0c197290f13934aa9541f1e12b097',
      fixReleasedIn: 'i18n-embed-fl 0.10.0',
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
  assert.match(cargoManifest, /^name = "i18n-embed-fl"$/mu)
  assert.match(cargoManifest, /^version = "0\.9\.4"$/mu)
  assert.match(cargoManifest, /^license = "MIT"$/mu)

  const patchRecord = sourceLock.patches.find(({ id }) => (
    id === 'i18n-embed-fl-stable-specified-argument-order'
  ))
  assert.deepEqual(patchRecord, {
    id: 'i18n-embed-fl-stable-specified-argument-order',
    file: vendoredSource.patch,
    planned: false,
    targets: ['upstream/vendor/i18n-embed-fl-0.9.4/src/lib.rs'],
  })

  const notices = readFileSync(join(root, 'THIRD_PARTY_NOTICES.md'), 'utf8')
  assert.match(notices, /## i18n-embed-fl/u)
  assert.match(notices, /Copyright 2020 Luke Frisken/u)
  assert.match(notices, /upstream\/vendor\/i18n-embed-fl-0\.9\.4\/LICENSE\.txt/u)
})

test('the exact patch reverses to the recorded crates.io source', t => {
  const temporaryRoot = mkdtempSync(join(tmpdir(), 'winwincode-i18n-embed-fl-'))
  t.after(() => rmSync(temporaryRoot, { force: true, recursive: true }))
  const cleanSource = join(temporaryRoot, 'i18n-embed-fl-0.9.4')
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

test('macro generation has stable compile-time order and keeps the runtime HashMap', () => {
  const source = readFileSync(join(vendorRoot, 'src', 'lib.rs'), 'utf8')
  assert.match(source, /specified_args: Vec<\(syn::LitStr, Box<syn::Expr>\)>/u)
  assert.match(source, /args\.sort_by_key\(\|\(s, _\)\| s\.value\(\)\)/u)
  assert.match(source, /for \(key, value\) in &specified_args/u)
  assert.match(source, /let mut args = std::collections::HashMap::new\(\)/u)
  assert.doesNotMatch(source, /specified_args: HashMap<syn::LitStr/u)
})

test('release source inventory includes the vendored source, patch and license', () => {
  const paths = new Set(releaseSourcePaths(root))
  for (const path of [
    'upstream/patches/i18n-embed-fl/0001-stable-specified-argument-order.patch',
    'upstream/vendor/i18n-embed-fl-0.9.4/Cargo.toml',
    'upstream/vendor/i18n-embed-fl-0.9.4/LICENSE.txt',
    'upstream/vendor/i18n-embed-fl-0.9.4/src/lib.rs',
    'upstream/vendor/i18n-embed-fl-0.9.4/tests/fl_macro.rs',
  ]) {
    assert.equal(paths.has(path), true, `${path} must be a release source input`)
  }
})
