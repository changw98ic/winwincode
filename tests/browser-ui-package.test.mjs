// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { existsSync } from 'node:fs'
import { mkdtemp, readFile, readdir, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  BrowserUiPackageReleaseError,
  buildBrowserUiPackageRelease,
} from '../packages/browser-ui/scripts/build-release.mjs'

const root = resolve(import.meta.dirname, '..')
const packageRoot = resolve(root, 'packages/browser-ui')
const sourceRoot = resolve(packageRoot, 'src')
const primitiveModules = Object.freeze([
  'button.ts',
  'error-state.ts',
  'mounted-view.ts',
  'page-header.ts',
  'panel.ts',
  'status-badge.ts',
])
const packedFiles = Object.freeze([
  'LICENSE',
  'dist/button.d.ts',
  'dist/button.js',
  'dist/error-state.d.ts',
  'dist/error-state.js',
  'dist/index.d.ts',
  'dist/index.js',
  'dist/mounted-view.d.ts',
  'dist/mounted-view.js',
  'dist/page-header.d.ts',
  'dist/page-header.js',
  'dist/panel.d.ts',
  'dist/panel.js',
  'dist/status-badge.d.ts',
  'dist/status-badge.js',
  'package.json',
])

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

async function outputDirectory(t) {
  const directory = await mkdtemp(join(tmpdir(), 'winwincode-browser-ui-release-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  return directory
}

async function filesBelow(directory) {
  const files = []
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) files.push(...await filesBelow(path))
    else if (entry.isFile()) files.push(path)
  }
  return files
}

test('browser-ui has one public code entry and owns exactly six primitive modules', async () => {
  const manifest = JSON.parse(await readFile(join(packageRoot, 'package.json'), 'utf8'))
  const rootManifest = JSON.parse(await readFile(join(root, 'package.json'), 'utf8'))
  assert.equal(manifest.name, '@winwincode/browser-ui')
  assert.equal(manifest.version, rootManifest.version)
  assert.deepEqual(Object.keys(manifest.exports), ['.', './package.json'])
  assert.deepEqual(manifest.files, ['dist'])
  assert.equal(manifest.publishConfig.access, 'public')
  assert.equal('dependencies' in manifest, false)

  const sourceFiles = (await readdir(sourceRoot)).sort()
  assert.deepEqual(sourceFiles, [...primitiveModules, 'index.ts'].sort())
  const sources = await Promise.all(sourceFiles.map(name => readFile(join(sourceRoot, name), 'utf8')))
  assert.equal(sources.every(source => source.includes('SPDX-License-Identifier: Apache-2.0')), true)
  assert.equal(
    sources.some(source => /(?:cloud|enterprise|saas|tenant|billing|generated|schema)/iu.test(source)),
    false,
  )
  assert.equal(sources.some(source => /(?:\.\.\/|file:|link:|workspace:)/u.test(source)), false)
})

test('the previous Client component paths and re-exports are removed', async () => {
  for (const module of primitiveModules) {
    assert.equal(existsSync(join(root, 'apps/client/src/components', module)), false)
  }
  const clientBarrel = await readFile(join(root, 'apps/client/src/components/index.ts'), 'utf8')
  for (const module of primitiveModules) {
    assert.equal(clientBarrel.includes(`./${module.replace(/\.ts$/u, '.js')}`), false)
  }
})

test('Community callers consume the exact browser-ui package entry', async () => {
  const clientManifest = JSON.parse(await readFile(join(root, 'apps/client/package.json'), 'utf8'))
  assert.equal(clientManifest.dependencies['@winwincode/browser-ui'], '0.1.0-alpha.1')
  const sourcePaths = (await filesBelow(join(root, 'apps/client/src')))
    .filter(path => path.endsWith('.ts'))
  const sources = await Promise.all(sourcePaths.map(async path => ({
    path,
    source: await readFile(path, 'utf8'),
  })))
  const oldEntry = /from ['"](?:\.\/|\.\.\/)components\/(?:button|error-state|mounted-view|page-header|panel|status-badge)\.js['"]/u
  assert.deepEqual(sources.filter(({ source }) => oldEntry.test(source)).map(({ path }) => path), [])
  for (const path of [
    'apps/client/src/settings-page.ts',
    'apps/client/src/device-page.ts',
  ]) {
    const source = await readFile(join(root, path), 'utf8')
    assert.equal(source.split("from '@winwincode/browser-ui'").length - 1, 1)
  }
})

test('release build records the exact package version, bytes, and SHA-256', async t => {
  const directory = await outputDirectory(t)
  const result = await buildBrowserUiPackageRelease({ outputDirectory: directory })
  const artifact = await readFile(result.artifactPath)
  const written = JSON.parse(await readFile(result.manifestPath, 'utf8'))

  assert.deepEqual(result.files, packedFiles)
  assert.equal(written.kind, 'winwincode.browser-ui-package-release-manifest.v1')
  assert.equal(written.state, 'package-built-not-published')
  assert.deepEqual(written.package, {
    name: '@winwincode/browser-ui',
    version: '0.1.0-alpha.1',
  })
  assert.equal(written.artifact.bytes, artifact.byteLength)
  assert.equal(written.artifact.sha256, sha256(artifact))
  assert.match(written.artifact.sha256, /^[0-9a-f]{64}$/u)
})

test('release output must be explicit and outside the repository', async () => {
  for (const options of [
    {},
    { outputDirectory: join(packageRoot, 'release') },
  ]) {
    await assert.rejects(
      buildBrowserUiPackageRelease(options),
      error => error instanceof BrowserUiPackageReleaseError,
    )
  }
})
