#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { chmodSync, copyFileSync, cpSync, mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'

import { deviceClientSbom } from './build-device-client-release.mjs'
import { canonicalJson, descriptorsBelow, sourceIdentity, targetConfiguration } from './release-artifact-contract.mjs'
import { verifyApiProductionSourceSeal } from './run-api-production-vertical.mjs'
import { scanReleaseArtifactContent } from './verify-release-artifact-security.mjs'

const root = process.cwd()
const producerCommit = execFileSync('git', ['rev-parse', 'HEAD'], {
  cwd: resolve(import.meta.dirname, '..'), encoding: 'utf8',
}).trim()
const [target, output] = process.argv.slice(2)
assert.equal(process.argv.length, 4, 'Usage: node scripts/stage-product-release.mjs TARGET OUTPUT')
const identity = targetConfiguration(target)
const sourceCommit = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim()
const sourceDateEpoch = Number(execFileSync('git', ['show', '-s', '--format=%ct', 'HEAD'], { cwd: root, encoding: 'utf8' }))
assert.equal(execFileSync('git', ['status', '--porcelain'], { cwd: root, encoding: 'utf8' }).trim(), '', 'release source must be clean')
const binaryRoot = resolve(root, 'target', target, 'release')
const artifactRoot = resolve(output)
assert.ok(artifactRoot.startsWith(`${resolve(root, 'release-artifacts')}/`), 'output must be inside release-artifacts/')
mkdirSync(artifactRoot, { recursive: false })
for (const directory of ['bin', 'legal', 'service', 'verification']) {
  mkdirSync(resolve(artifactRoot, directory))
}
const binaries = ['winwincode-server', 'winwincode-worker', 'winwincode-kernel-helper', 'wwc']
for (const binary of binaries) {
  const source = resolve(binaryRoot, binary)
  assert.equal(statSync(source).mode & 0o777, 0o755, `${binary} must have mode 0755`)
  copyFileSync(source, resolve(artifactRoot, 'bin', binary))
}
const helperManifest = 'winwincode-kernel-helper.release.json'
copyFileSync(resolve(binaryRoot, helperManifest), resolve(artifactRoot, 'bin', helperManifest))
chmodSync(resolve(artifactRoot, 'bin', helperManifest), 0o644)
const seal = verifyApiProductionSourceSeal({
  root,
  serverBinary: resolve(binaryRoot, 'winwincode-server'),
  helperExecutable: resolve(binaryRoot, 'winwincode-kernel-helper'),
})
const apiReport = JSON.parse(readFileSync(resolve(root, '.cache/release-api-report.json'), 'utf8'))
assert.equal(apiReport.execution, 'device-worker-only')
assert.equal(apiReport.devicePath?.complete, true)
assert.equal(apiReport.health?.initial, 'ready')
assert.equal(apiReport.health?.afterRestart, 'ready')
assert.equal(apiReport.flow?.chat?.status, 'Completed')
assert.equal(apiReport.flow?.cancel?.state, 'cancelled')
assert.equal(apiReport.flow?.strongflow?.status, 'done')
assert.equal(apiReport.flow?.strongflow?.verdictStatus, 'pass')
assert.equal(apiReport.deterministic?.contentEqual, true)
assert.equal(apiReport.restart?.messageBytesStable, true)
assert.equal(apiReport.restart?.deliveryBytesStable, true)
assert.equal(apiReport.artifacts?.sourceSeal?.serverBinarySha256, seal.seal.serverBinarySha256)
cpSync(resolve(root, 'apps/client/dist/public'), resolve(artifactRoot, 'client'), { recursive: true })
const clientVersion = JSON.parse(readFileSync(resolve(artifactRoot, 'client/version.json'), 'utf8'))
const source = sourceIdentity(root, sourceCommit, sourceDateEpoch)
assert.equal(clientVersion.version, source.version)
for (const name of ['LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md']) {
  copyFileSync(resolve(root, name), resolve(artifactRoot, 'legal', name))
}
const service = identity.os === 'macos' ? 'dev.winwincode.device-client.plist' : 'winwincode-device-client.service'
copyFileSync(resolve(root, 'deploy/device-client', service), resolve(artifactRoot, 'service', service))
writeFileSync(resolve(artifactRoot, 'verification/api-production.json'), canonicalJson({
  schemaVersion: apiReport.schemaVersion,
  execution: apiReport.execution,
  devicePathComplete: apiReport.devicePath.complete,
  health: apiReport.health,
  chatStatus: apiReport.flow.chat.status,
  cancelState: apiReport.flow.cancel.state,
  strongflowStatus: apiReport.flow.strongflow.status,
  verdictStatus: apiReport.flow.strongflow.verdictStatus,
  repeatedContentEqual: apiReport.deterministic.contentEqual,
  messageBytesStable: apiReport.restart.messageBytesStable,
  deliveryBytesStable: apiReport.restart.deliveryBytesStable,
  sourceSeal: apiReport.artifacts.sourceSeal,
}))
writeFileSync(resolve(artifactRoot, 'verification/source-seal.json'), canonicalJson(seal.seal))
writeFileSync(resolve(artifactRoot, 'sbom.json'), canonicalJson(deviceClientSbom(root)))
const files = descriptorsBelow(artifactRoot)
for (const file of files) {
  if (file.path.startsWith('legal/') || file.path === 'client/THIRD_PARTY_NOTICES.md') continue
  const findings = scanReleaseArtifactContent({
    bytes: readFileSync(resolve(artifactRoot, file.path)),
    target,
    path: file.path,
    binary: binaries.some(binary => file.path === `bin/${binary}`) || /\.(?:js|css)$/u.test(file.path),
  })
  assert.deepEqual(findings, [], `credential scan rejected ${file.path}`)
}
const manifest = {
  schemaVersion: 1,
  kind: 'winwincode.product-release.v1',
  producerCommit,
  source,
  target: identity,
  helperReleasePublicKeyHex: seal.seal.helperReleasePublicKeyHex,
  checks: ['cargo-locked-release-build', 'api-production-vertical', 'credential-content-scan', 'sha256-file-inventory'],
  files,
}
const manifestText = canonicalJson(manifest)
writeFileSync(resolve(artifactRoot, 'product-release-manifest.json'), manifestText)
const sums = [...files, {
  path: 'product-release-manifest.json',
  sha256: createHash('sha256').update(manifestText).digest('hex'),
}].map(file => `${file.sha256}  ${file.path}\n`).join('')
writeFileSync(resolve(artifactRoot, 'SHA256SUMS'), sums)
process.stdout.write(`staged WinWinCode ${source.version} for ${target}\n`)
