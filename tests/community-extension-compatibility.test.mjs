import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import test from 'node:test'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const matrix = JSON.parse(readFileSync(
  join(root, 'docs/contracts/community-extension-compatibility.matrix.json'),
  'utf8',
))

function files(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name)
    return entry.isDirectory() ? files(path) : [path]
  })
}

function treeSha256(directory) {
  const hash = createHash('sha256')
  for (const path of files(directory).sort()) {
    hash.update(relative(directory, path))
    hash.update('\0')
    hash.update(readFileSync(path))
    hash.update('\0')
  }
  return `sha256:${hash.digest('hex')}`
}

test('extension samples have immutable source, digest, platform, host, and license metadata', () => {
  const upstream = JSON.parse(readFileSync(join(root, 'upstream/sources.lock.json'), 'utf8'))
  assert.equal(matrix.source.tag, upstream.codex.tag)
  assert.equal(matrix.source.version, upstream.codex.version)
  assert.equal(matrix.source.commit, upstream.codex.commit)
  assert.equal(matrix.source.archiveSha256, `sha256:${upstream.codex.archiveSha256}`)
  assert.deepEqual(matrix.classifications, [
    'direct_import',
    'adapt_then_support',
    'reference_only',
    'unsupported',
  ])
  assert.equal(matrix.samples.length, 6)
  for (const sample of matrix.samples) {
    const directory = join(root, sample.sourceDirectory)
    assert.equal(statSync(directory).isDirectory(), true, sample.id)
    assert.equal(treeSha256(directory), sample.treeSha256, sample.id)
    assert.equal(readFileSync(join(root, sample.licenseEvidence), 'utf8').includes('Apache License'), true)
    assert.equal(sample.licenseSpdx, 'Apache-2.0')
    assert.equal(typeof sample.platform, 'string')
    assert.equal(sample.hostDependencies.length > 0, true)
    assert.equal(matrix.classifications.includes(sample.classification), true)
    assert.equal(sample.verification, 'unverified')
  }
})

test('preflight is read-only and unverified extensions remain non-executable', () => {
  assert.equal(matrix.policy.discovery, 'metadata-only-no-execution')
  assert.equal(matrix.policy.validationIsSecurityCertification, false)
  assert.equal(matrix.policy.unknownPackage, 'unverified-and-not-executable')
  assert.match(matrix.policy.installation, /^not-enabled-/u)
  assert.equal(matrix.samples.every(sample => sample.decision === 'investigate'), true)

  const firstPartyRuntimeFiles = execFileSync(
    'git',
    ['ls-files', '-z', '--', 'apps', 'crates', 'packages'],
    { cwd: root },
  ).toString('utf8').split('\0').filter(path => /\.(?:rs|ts|js|mjs)$/u.test(path))
  for (const path of firstPartyRuntimeFiles) {
    if (!existsSync(join(root, path))) continue
    assert.doesNotMatch(
      readFileSync(join(root, path), 'utf8'),
      /skills\/src\/assets\/samples/u,
      path,
    )
  }
})

test('MCP and frontend compatibility keep authority and failure boundaries explicit', () => {
  assert.equal(matrix.policy.mcpInvocation, 'worker-capability-catalog-then-action-gateway')
  assert.equal(matrix.policy.unmanagedCapability, 'deny')
  assert.equal(matrix.policy.fullCordisRuntime, 'unsupported')
  assert.equal(matrix.policy.arbitraryDomOrAuthorityAccess, 'unsupported')
  assert.equal(matrix.policy.serverAuthorizationDependsOnPluginUi, false)

  const capability = readFileSync(
    join(root, 'crates/winwincode-execution-port/src/capability_adapter.rs'),
    'utf8',
  )
  assert.match(capability, /MappedPluginManifest/u)
  assert.match(capability, /UnmanagedCapabilityPolicy::Deny/u)
  assert.match(capability, /WorkerActionGateway/u)

  const page = readFileSync(join(root, 'apps/client/src/extensions-page.ts'), 'utf8')
  assert.match(page, /presentation sample data/u)
  assert.match(page, /No install\/add contract exists yet/u)
  assert.match(page, /disabled: true/u)
  assert.doesNotMatch(page, /submitCommand|WorkerActionGateway|innerHTML\s*=.*plugin/gu)

  const representatives = Object.fromEntries(
    matrix.representatives.map(entry => [entry.id, entry]),
  )
  assert.equal(representatives['embedded-codex-mcp-capability'].verification, 'verified')
  assert.equal(representatives['dsh-cordis-backend'].classification, 'unsupported')
  assert.equal(representatives['dsh-interface-plugin'].classification, 'reference_only')
  assert.equal(representatives['community-card-viewer-registration'].verification, 'verified-absent')
})
