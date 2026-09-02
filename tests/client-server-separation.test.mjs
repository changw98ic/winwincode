import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')

function source(path) {
  return readFileSync(join(root, path), 'utf8')
}

test('static Client permits only secure remote Control Plane connections', () => {
  const index = source('apps/client/public/index.html')
  const contentSecurityPolicy = index.match(
    /http-equiv="Content-Security-Policy" content="([^"]+)"/u,
  )?.[1]
  assert.ok(contentSecurityPolicy)
  for (const directive of [
    "default-src 'self'",
    "script-src 'self'",
    "style-src 'self'",
    'connect-src https: wss:',
    "object-src 'none'",
    "base-uri 'none'",
    "form-action 'none'",
  ]) assert.ok(contentSecurityPolicy.includes(directive), directive)

  const runtimeConfig = source('apps/client/public/runtime-config.js')
  assert.match(runtimeConfig, /serverUrl/u)
  assert.doesNotMatch(runtimeConfig, /token|authorization|cookie|https?:\/\/|wss?:\/\//iu)
})

test('Client and Server expose independent version and rollback coordinates', () => {
  const rootPackage = JSON.parse(source('package.json'))
  const clientPackage = JSON.parse(source('apps/client/package.json'))
  const workspaceManifest = source('Cargo.toml')
  const server = source('crates/winwincode-server/src/server.rs')
  const build = source('apps/client/scripts/build.mjs')
  const releaseArtifacts = source('scripts/release-artifact-contract.mjs')

  const serverVersion = workspaceManifest.match(/\[workspace\.package\][\s\S]*?version = "([^"]+)"/u)?.[1]
  assert.ok(serverVersion)
  assert.equal(clientPackage.version, rootPackage.version)
  assert.equal(serverVersion, rootPackage.version)
  assert.match(server, /"serverVersion": env!\("CARGO_PKG_VERSION"\)/u)
  assert.match(server, /"schemaVersion": SUPPORTED_SCHEMA_VERSION/u)
  assert.match(build, /version: manifest\.version/u)
  assert.match(build, /controlPlaneSchemaVersion: 'winwincode\/v1'/u)
  assert.match(build, /mutableAtDeployment: true/u)
  assert.match(build, /!\['asset-manifest\.json', 'runtime-config\.js'\]\.includes\(path\)/u)
  assert.match(releaseArtifacts, /rust: Object\.freeze\(rust\)/u)
  assert.match(releaseArtifacts, /package: '@winwincode\/client'/u)
  assert.match(releaseArtifacts, /files: Object\.freeze\(client\)/u)
})

test('browser boundary has one serverUrl and no internal Worker or Provider route', () => {
  const facade = source('apps/client/src/control-plane-client.ts')
  const server = source('crates/winwincode-server/src/server.rs')

  assert.match(facade, /readonly serverUrl: string/u)
  assert.match(facade, /webSocket\.protocol = parsed\.protocol === 'https:' \? 'wss:' : 'ws:'/u)
  assert.match(facade, /credentials: 'include'/u)
  assert.doesNotMatch(facade, /[?&](?:token|authorization|credential)=/iu)
  for (const endpoint of ['/api/v1/commands', '/api/v1/queries', '/api/v1/events']) {
    assert.match(server, new RegExp(endpoint.replaceAll('/', '\\/'), 'u'))
  }
  assert.doesNotMatch(server, /route\("\/(?:internal\/)?(?:workers|providers)/u)
})
