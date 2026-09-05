import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import {
  readFileSync,
  readdirSync,
} from 'node:fs'
import { join, relative, resolve } from 'node:path'
import test from 'node:test'

import { pnpmPackDryRun } from '../scripts/pnpm-pack-report.mjs'

const root = resolve(import.meta.dirname, '..')
const clientRoot = join(root, 'apps', 'client')
const publicRoot = join(clientRoot, 'dist', 'public')
const targets = [
  'aarch64-apple-darwin',
  'x86_64-apple-darwin',
  'aarch64-unknown-linux-gnu',
  'x86_64-unknown-linux-gnu',
]

function buildClient() {
  const result = spawnSync(
    'corepack',
    ['pnpm', '--filter', '@winwincode/client', 'build'],
    { cwd: root, encoding: 'utf8' },
  )
  assert.equal(
    result.status,
    0,
    `independent Client build failed:\n${result.stdout}${result.stderr}`,
  )
}

function filesBelow(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name)
    return entry.isDirectory() ? filesBelow(path) : [path]
  })
}

function fileDigests(directory) {
  return Object.fromEntries(filesBelow(directory)
    .map(path => [
      relative(directory, path).replaceAll('\\', '/'),
      createHash('sha256').update(readFileSync(path)).digest('hex'),
    ])
    .sort(([left], [right]) => left.localeCompare(right)))
}

test('independent Client build produces one deterministic target-neutral application package', () => {
  buildClient()
  const first = fileDigests(join(clientRoot, 'dist'))
  buildClient()
  assert.deepEqual(fileDigests(join(clientRoot, 'dist')), first)

  const packageManifest = JSON.parse(readFileSync(join(clientRoot, 'package.json'), 'utf8'))
  const assetManifest = JSON.parse(
    readFileSync(join(publicRoot, 'asset-manifest.json'), 'utf8'),
  )
  const version = JSON.parse(readFileSync(join(publicRoot, 'version.json'), 'utf8'))
  assert.equal(assetManifest.package, '@winwincode/client')
  assert.equal(assetManifest.version, packageManifest.version)
  assert.equal(version.version, packageManifest.version)
  assert.equal(version.controlPlaneSchemaVersion, 'winwincode/v1')
  assert.deepEqual(assetManifest.runtimeConfig, {
    path: 'runtime-config.js',
    field: 'serverUrl',
    mutableAtDeployment: true,
  })
  assert.equal(assetManifest.assets.some(asset => asset.path === 'runtime-config.js'), false)
  assert.deepEqual(Object.keys(assetManifest.targets), targets)
  for (const target of targets) assert.deepEqual(assetManifest.targets[target], assetManifest.assets)

  for (const asset of assetManifest.assets) {
    const bytes = readFileSync(join(publicRoot, asset.path))
    assert.equal(asset.bytes, bytes.length, asset.path)
    assert.equal(asset.sha256, createHash('sha256').update(bytes).digest('hex'), asset.path)
  }
  for (const routeChunk of [
    'enterprise-application-',
    'enterprise-resource-page-',
    'enterprise-operations-page-',
  ]) {
    assert.equal(
      assetManifest.assets.some(asset => asset.path.startsWith(`assets/${routeChunk}`)),
      true,
      `${routeChunk} route chunk`,
    )
  }
})

test('deployable Client files contain only browser assets and runtime serverUrl configuration', () => {
  buildClient()
  const packed = pnpmPackDryRun(clientRoot)
  for (const required of [
    'dist/module/index.js',
    'dist/module/index.d.ts',
    'dist/module/auth-view-model.js',
    'dist/module/auth-view-model.d.ts',
    'dist/module/auth-page.js',
    'dist/module/auth-page.d.ts',
    'dist/module/chat-view-model.js',
    'dist/module/chat-view-model.d.ts',
    'dist/module/chat-page.js',
    'dist/module/chat-page.d.ts',
    'dist/module/strongflow-view-model.js',
    'dist/module/strongflow-view-model.d.ts',
    'dist/module/strongflow-page.js',
    'dist/module/strongflow-page.d.ts',
    'dist/module/strongflow-diagrams.js',
    'dist/module/strongflow-candidate.js',
    'dist/module/enterprise-application.js',
    'dist/module/enterprise-application.d.ts',
    'dist/module/enterprise-resource-page.js',
    'dist/module/enterprise-operations-page.js',
    'dist/public/index.html',
    'dist/public/runtime-config.js',
    'dist/public/version.json',
    'dist/public/asset-manifest.json',
    'dist/public/assets/client.js',
    'dist/public/assets/client.css',
  ]) assert.ok(packed.includes(required), required)
  assert.equal(packed.every(path => !/(?:^|\/)(?:node_modules|target|prebuild)(?:\/|$)/u.test(path)), true)
  assert.equal(packed.every(path => !/\.(?:node|dylib|so|exe)$/u.test(path)), true)
  assert.deepEqual(JSON.parse(readFileSync(join(clientRoot, 'package.json'), 'utf8')).dependencies, undefined)

  const runtimeConfig = readFileSync(join(publicRoot, 'runtime-config.js'), 'utf8')
  assert.match(runtimeConfig, /serverUrl/u)
  assert.doesNotMatch(runtimeConfig, /https?:\/\/|wss?:\/\//u)
  const browserBundle = readFileSync(join(publicRoot, 'assets', 'client.js'), 'utf8')
  assert.doesNotMatch(browserBundle, /node:|@deepseek-ai\/cordis|process\.env/u)

  const buildScript = readFileSync(join(clientRoot, 'scripts', 'build.mjs'), 'utf8')
  assert.doesNotMatch(buildScript, /cargo|build-native|winwincode-server|winwincode-worker/u)
})

test('Client shell has one facade and the six canonical product entries with Home first', () => {
  const application = readFileSync(join(clientRoot, 'src', 'application.ts'), 'utf8')
  const surfaces = readFileSync(join(clientRoot, 'src', 'client-surface.ts'), 'utf8')
  const index = readFileSync(join(clientRoot, 'src', 'index.ts'), 'utf8')
  for (const surface of ['home', 'chat', 'strongflow', 'settings', 'attention', 'enterprise']) {
    assert.match(surfaces, new RegExp(`id: '${surface}'`, 'u'))
  }
  // UI-504: the Attention-first dashboard is the canonical first screen, so a
  // start-up without a route never lands in an arbitrary Chat or Delivery.
  assert.match(surfaces, /id: 'home'[\s\S]+default: true/u)
  assert.match(surfaces, /id: 'home'[\s\S]+default: true[\s\S]+id: 'chat'/u)
  assert.match(application, /activeSurface\.id === 'home'/u)
  // The factory seam grows with each authenticated surface (login, clients,
  // repositories); what must stay stable is that every surface resolves it
  // through the single control-plane-client facade.
  assert.equal((`${application}\n${index}`.match(/createControlPlaneClient/g) ?? []).length, 4)
  assert.doesNotMatch(application, /\bfetch\s*\(|new\s+WebSocket/u)
})
