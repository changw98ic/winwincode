import assert from 'node:assert/strict'
import { createHash, generateKeyPairSync, verify } from 'node:crypto'
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  CommunityCoreFinalizeError,
  finalizeCommunityCoreRelease,
} from '../scripts/finalize-community-core-release.mjs'
import { verifyCommunityCoreRelease } from '../scripts/verify-community-core-release.mjs'

const repositoryRoot = resolve(import.meta.dirname, '..')
const releaseContract = JSON.parse(readFileSync(
  resolve(repositoryRoot, 'docs/decisions/0031-community-core-release.json'),
  'utf8',
))
const version = JSON.parse(readFileSync(resolve(repositoryRoot, 'package.json'), 'utf8')).version
const sourceCommit = 'a'.repeat(40)
const sourceDateEpoch = 1_789_228_424

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

function inputNames(contract) {
  return [
    ...contract.targetState.consumableRustCrates.map(({ name }) => `${name}-${version}.crate`),
    ...contract.targetState.consumableNpmPackages.map(({ name }) => (
      `${name.slice(1).replace('/', '-')}-${version}.tgz`
    )),
    contract.targetState.contractBundle.fileName.replace('{version}', version),
    ...contract.targetState.workerRuntimeBundles.targets.map(target => (
      contract.targetState.workerRuntimeBundles.fileName
        .replace('{version}', version)
        .replace('{target}', target)
    )),
  ].toSorted()
}

function fixture(t) {
  const base = mkdtempSync(join(tmpdir(), 'winwincode-core-finalize-'))
  t.after(() => rmSync(base, { recursive: true, force: true }))
  const root = join(base, 'root')
  const inputRoot = join(base, 'inputs')
  mkdirSync(join(root, 'release-keys'), { recursive: true })
  mkdirSync(inputRoot, { recursive: true })
  writeFileSync(join(root, 'package.json'), `${JSON.stringify({ version })}\n`)

  const { privateKey, publicKey } = generateKeyPairSync('ed25519')
  const publicKeyDer = publicKey.export({ type: 'spki', format: 'der' })
  const publicKeyPath = 'release-keys/community-core-ed25519-public.pem'
  writeFileSync(join(root, publicKeyPath), publicKey.export({ type: 'spki', format: 'pem' }))
  const contract = structuredClone(releaseContract)
  contract.targetState.releaseManifest.signature.publicKeyFile = publicKeyPath
  contract.targetState.releaseManifest.signature.publicKeySha256 = sha256(publicKeyDer)
  const contractPath = join(root, 'contract.json')
  writeFileSync(contractPath, `${JSON.stringify(contract)}\n`)
  for (const name of inputNames(contract)) writeFileSync(join(inputRoot, name), `fixture:${name}\n`)
  return {
    base,
    contract,
    contractPath,
    inputRoot,
    privateKeyPem: privateKey.export({ type: 'pkcs8', format: 'pem' }).toString(),
    publicKey,
    root,
  }
}

test('final Community Core release is exact, deterministic, and signed', t => {
  const setup = fixture(t)
  const first = join(setup.base, 'release-one')
  const second = join(setup.base, 'release-two')
  const options = {
    root: setup.root,
    contractPath: setup.contractPath,
    inputRoot: setup.inputRoot,
    sourceCommit,
    sourceDateEpoch,
    privateKeyPem: setup.privateKeyPem,
  }
  const result = finalizeCommunityCoreRelease({ ...options, outputRoot: first })
  finalizeCommunityCoreRelease({ ...options, outputRoot: second })

  assert.equal(result.artifactCount, 19)
  assert.deepEqual(readdirSync(first).toSorted(), readdirSync(second).toSorted())
  for (const name of readdirSync(first)) {
    assert.deepEqual(readFileSync(join(first, name)), readFileSync(join(second, name)), name)
  }
  const manifestBytes = readFileSync(join(first, 'community-core-release-manifest.json'))
  const manifest = JSON.parse(manifestBytes)
  assert.equal(manifest.version, version)
  assert.equal(manifest.sourceCommit, sourceCommit)
  assert.equal(manifest.artifacts.cargo.length, 9)
  assert.equal(manifest.artifacts.npm.length, 5)
  assert.equal(manifest.artifacts.workerRuntimes.length, 4)
  assert.equal(manifest.artifacts.workerRuntimes.some(({ name }) => name.includes('server')), false)
  const signature = Buffer.from(
    readFileSync(join(first, 'community-core-release-manifest.json.sig'), 'utf8').trim(),
    'base64url',
  )
  assert.equal(verify(null, manifestBytes, setup.publicKey, signature), true)
  const checksums = readFileSync(join(first, 'SHA256SUMS'), 'utf8').trim().split('\n')
  assert.equal(checksums.length, readdirSync(first).length - 1)
  assert.equal(checksums.some(line => line.endsWith('  community-core-release-manifest.json')), true)
  assert.equal(checksums.some(line => line.endsWith('  community-core-release-manifest.json.sig')), true)
  assert.equal(verifyCommunityCoreRelease({
    root: setup.root,
    contractPath: setup.contractPath,
    inputRoot: first,
    expectedTag: `core-v${version}`,
  }).artifactCount, 19)

  writeFileSync(join(first, 'community-core-release-manifest.json.sig'), 'tampered\n')
  assert.throws(
    () => verifyCommunityCoreRelease({
      root: setup.root,
      contractPath: setup.contractPath,
      inputRoot: first,
      expectedTag: `core-v${version}`,
    }),
    /SHA-256 mismatch/u,
  )
})

test('final Community Core release rejects any extra product file', t => {
  const setup = fixture(t)
  writeFileSync(join(setup.inputRoot, 'winwincode-server'), 'not core\n')
  assert.throws(
    () => finalizeCommunityCoreRelease({
      root: setup.root,
      contractPath: setup.contractPath,
      inputRoot: setup.inputRoot,
      outputRoot: join(setup.base, 'release'),
      sourceCommit,
      sourceDateEpoch,
      privateKeyPem: setup.privateKeyPem,
    }),
    error => error instanceof CommunityCoreFinalizeError
      && error.code === 'CORE_ARTIFACT_SET_INVALID',
  )
})
