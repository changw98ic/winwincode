#!/usr/bin/env node

/**
 * API-only production vertical for the standalone Server.
 *
 * This runner deliberately talks to the generated HTTP command/query
 * endpoints. It does not import a UI, start an interactive page, or manufacture
 * runtime/Delivery projections in JavaScript. The only values asserted here
 * are values returned by the public API.
 */

import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import {
  createHash,
  createPrivateKey,
  createPublicKey,
  randomBytes,
  sign,
  verify,
} from 'node:crypto'
import { createServer as createNetServer } from 'node:net'
import { request as httpsRequest } from 'node:https'
import {
  appendFileSync,
  chmodSync,
  copyFileSync,
  closeSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  readSync,
  renameSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { basename, dirname, join, resolve } from 'node:path'
import { tmpdir } from 'node:os'
import { pathToFileURL } from 'node:url'

import { projectSourceDigest } from './product-build-contract.mjs'

const ROOT = resolve(import.meta.dirname, '..')
const SCHEMA_VERSION = 'winwincode/v1'
const DEFAULT_TIMEOUT_MILLIS = 240_000
const POLL_INTERVAL_MILLIS = 50
const SERVER_GRACEFUL_STOP_TIMEOUT_MILLIS = 30_000
const MAX_DELIVERY_TRANSITIONS = 64
const HELPER_RELEASE_MANIFEST_NAME = 'winwincode-kernel-helper.release.json'
const HELPER_RELEASE_BINARY_NAME = 'winwincode-kernel-helper'
const HELPER_RELEASE_BINARY_MODE = 0o755
const TEST_HELPER_RELEASE_PRIVATE_KEY_HEX = '2a'.repeat(32)
const API_SOURCE_SEAL_NAME = 'winwincode-api-production.source.json'
const API_SOURCE_SEAL_PROTOCOL = 'winwincode-api-production-source-seal'
const API_SOURCE_SEAL_VERSION = 1
const API_SOURCE_SEAL_MAX_BYTES = 32 * 1024
const API_RUNNER_SOURCE_PATH = 'scripts/run-api-production-vertical.mjs'
const API_SOURCE_TRACKED_PATHS = [
  'Cargo.lock',
  'Cargo.toml',
  'rust-toolchain.toml',
  'crates',
  API_RUNNER_SOURCE_PATH,
  'scripts/product-build-contract.mjs',
]
const API_SOURCE_SEAL_KEYS = [
  'gitHead',
  'helperBinaryMode',
  'helperBinaryPath',
  'helperBinarySha256',
  'helperReleaseManifestMode',
  'helperReleaseManifestPath',
  'helperReleaseManifestSha256',
  'helperReleasePublicKeyHex',
  'protocol',
  'schemaVersion',
  'serverBinaryMode',
  'serverBinaryPath',
  'serverBinarySha256',
  'sourceSha256',
  'trackedDiffSha256',
  'version',
].toSorted()
const HELPER_RELEASE_MANIFEST_KEYS = [
  'binaryMode',
  'binaryPath',
  'binarySha256',
  'packageVersion',
  'protocol',
  'schemaVersion',
  'signature',
  'sourceSha256',
  'version',
].toSorted()

const IDS = Object.freeze({
  actor: 'usr_01J00000000000000000000000',
  organization: 'org_01J00000000000000000000000',
  workspace: 'wsp_01J00000000000000000000000',
  project: 'prj_01J00000000000000000000000',
  repository: 'rep_01J00000000000000000000000',
  session: 'psn_01J00000000000000000000001',
  repeatSession: 'psn_01J00000000000000000000002',
  cancelSession: 'psn_01J00000000000000000000003',
  delivery: 'dlv_01J00000000000000000000001',
  credential: 'crd_01J00000000000000000000001',
})

const ACTOR = Object.freeze({ kind: 'user', id: IDS.actor })
const SCOPE = Object.freeze({
  kind: 'repository',
  organizationId: IDS.organization,
  workspaceId: IDS.workspace,
  projectId: IDS.project,
  repositoryId: IDS.repository,
})

function configuredModelRoute(serverEnvironment = {}) {
  return {
    providerId: serverEnvironment.WWC_SERVER_MODEL_PROVIDER_ID ?? 'winwincode-loopback',
    modelId: serverEnvironment.WWC_SERVER_MODEL_ID ?? 'loopback-model',
    credentialReferenceId:
      serverEnvironment.WWC_SERVER_MODEL_CREDENTIAL_REFERENCE_ID ?? IDS.credential,
  }
}

function fail(message) {
  throw new Error(message)
}

function id(prefix, sequence) {
  return `${prefix}_${String(sequence).padStart(26, '0')}`
}

function page(limit = 200) {
  return { cursor: null, limit }
}

function helperReleaseSigningBytes(fields) {
  return Buffer.from([
    'winwincode-kernel-helper.release.v1',
    String(fields.schemaVersion),
    fields.protocol,
    String(fields.version),
    fields.packageVersion,
    fields.sourceSha256,
    fields.binarySha256,
    fields.binaryPath,
    String(fields.binaryMode),
  ].join('\0'))
}

function helperReleasePrivateKey(hex) {
  assert.match(hex, /^[0-9a-f]{64}$/u, 'helper release private key must be 32 lowercase bytes')
  return createPrivateKey({
    key: Buffer.from(`302e020100300506032b657004220420${hex}`, 'hex'),
    format: 'der',
    type: 'pkcs8',
  })
}

function helperReleasePublicKeyHex(privateKey) {
  return createPublicKey(privateKey).export({ format: 'der', type: 'spki' })
    .subarray(-32)
    .toString('hex')
}

function configuredHelperReleaseKey() {
  const privateKeyHex = process.env.WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX
    ?? TEST_HELPER_RELEASE_PRIVATE_KEY_HEX
  const privateKey = helperReleasePrivateKey(privateKeyHex)
  const derivedPublicKeyHex = helperReleasePublicKeyHex(privateKey)
  const configuredPublicKeyHex = process.env.WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX
    ?? derivedPublicKeyHex
  assert.equal(
    configuredPublicKeyHex,
    derivedPublicKeyHex,
    'helper release public key must match the configured private key',
  )
  return { privateKey, publicKeyHex: derivedPublicKeyHex }
}

function sha256File(path) {
  const hash = createHash('sha256')
  const descriptor = openSync(path, 'r')
  const buffer = Buffer.allocUnsafe(1024 * 1024)
  let offset = 0
  try {
    for (;;) {
      const bytesRead = readSync(descriptor, buffer, 0, buffer.length, offset)
      if (bytesRead === 0) break
      hash.update(buffer.subarray(0, bytesRead))
      offset += bytesRead
    }
  } finally {
    closeSync(descriptor)
  }
  return hash.digest('hex')
}

function fileIdentity(path, label) {
  let metadata
  try {
    metadata = lstatSync(path)
  } catch (error) {
    fail(`${label} is missing: ${path} (${error.message})`)
  }
  assert.equal(metadata.isSymbolicLink(), false, `${label} must not be a symlink`)
  assert.equal(metadata.isFile(), true, `${label} must be a regular file`)
  return {
    mode: metadata.mode & 0o777,
    sha256: `sha256:${sha256File(path)}`,
  }
}

function runGit(root, arguments_, encoding = 'utf8') {
  const result = spawnSync('git', arguments_, {
    cwd: root,
    encoding,
    maxBuffer: 512 * 1024 * 1024,
  })
  if (result.error !== undefined) throw result.error
  assert.equal(
    result.status,
    0,
    `source seal Git command failed: git ${arguments_.join(' ')}`,
  )
  return result.stdout
}

function sourceGitIdentity(root) {
  const gitHead = runGit(root, ['rev-parse', '--verify', 'HEAD']).trim()
  assert.match(gitHead, /^[0-9a-f]{40,64}$/u, 'source seal Git HEAD is invalid')
  const trackedDiff = runGit(root, [
    'diff',
    '--binary',
    'HEAD',
    '--',
    ...API_SOURCE_TRACKED_PATHS,
  ], 'buffer')
  assert.equal(Buffer.isBuffer(trackedDiff), true, 'source seal Git diff must be bytes')
  return {
    gitHead,
    trackedDiffSha256: `sha256:${createHash('sha256').update(trackedDiff).digest('hex')}`,
  }
}

function sourceTreeIdentity(root) {
  const git = sourceGitIdentity(root)
  return {
    sourceSha256: `sha256:${apiProductionSourceDigest(root)}`,
    ...git,
  }
}

/**
 * Digest the Rust build inputs and this runner itself.  The helper source is
 * included by projectSourceDigest, while the runner and its build contract are
 * included explicitly so an old target cannot be used with a changed API gate.
 */
export function apiProductionSourceDigest(root = ROOT) {
  const runnerPath = join(root, API_RUNNER_SOURCE_PATH)
  return createHash('sha256')
    .update('winwincode.api-production-source.v1')
    .update('\0')
    .update(projectSourceDigest(root))
    .update('\0')
    .update(API_RUNNER_SOURCE_PATH)
    .update('\0')
    .update(sha256File(runnerPath))
    .update('\0')
    .update('scripts/product-build-contract.mjs')
    .update('\0')
    .update(sha256File(join(root, 'scripts/product-build-contract.mjs')))
    .digest('hex')
}

function writeJsonAtomically(path, value) {
  const temporaryPath = `${path}.tmp-${process.pid}-${randomBytes(8).toString('hex')}`
  try {
    writeFileSync(temporaryPath, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o644 })
    renameSync(temporaryPath, path)
  } finally {
    if (existsSync(temporaryPath)) rmSync(temporaryPath, { force: true })
  }
}

function publicKeyFromHex(publicKeyHex) {
  assert.match(publicKeyHex, /^[0-9a-f]{64}$/u, 'source seal helper public key is invalid')
  return createPublicKey({
    key: Buffer.concat([
      Buffer.from('302a300506032b6570032100', 'hex'),
      Buffer.from(publicKeyHex, 'hex'),
    ]),
    format: 'der',
    type: 'spki',
  })
}

function readAndValidateHelperReleaseManifest({
  root,
  helperExecutable,
  manifestPath,
  publicKeyHex,
}) {
  const manifestIdentity = fileIdentity(manifestPath, 'helper release manifest')
  let manifest
  try {
    manifest = JSON.parse(readFileSync(manifestPath, 'utf8'))
  } catch (error) {
    fail(`helper release manifest is not valid JSON: ${error.message}`)
  }
  assert.deepEqual(
    Object.keys(manifest).toSorted(),
    HELPER_RELEASE_MANIFEST_KEYS,
    'helper release manifest fields are not canonical',
  )
  assert.equal(manifest.schemaVersion, 1)
  assert.equal(manifest.protocol, 'winwincode-kernel-helper-release')
  assert.equal(manifest.version, 1)
  assert.equal(manifest.packageVersion, '0.0.0')
  assert.equal(manifest.binaryPath, HELPER_RELEASE_BINARY_NAME)
  assert.equal(manifest.binaryMode, HELPER_RELEASE_BINARY_MODE)
  assert.match(manifest.sourceSha256, /^sha256:[0-9a-f]{64}$/u)
  assert.match(manifest.binarySha256, /^sha256:[0-9a-f]{64}$/u)
  assert.match(manifest.signature, /^[A-Za-z0-9_-]{86}$/u)
  const expectedSourceSha256 = `sha256:${sha256File(
    join(root, 'crates', 'helper', 'src', 'main.rs'),
  )}`
  assert.equal(
    manifest.sourceSha256,
    expectedSourceSha256,
    'helper release manifest source identity is stale',
  )
  const helperIdentity = fileIdentity(helperExecutable, 'kernel helper binary')
  assert.equal(
    manifest.binarySha256,
    helperIdentity.sha256,
    'helper release manifest binary identity is stale',
  )
  assert.equal(
    verify(
      null,
      helperReleaseSigningBytes(manifest),
      publicKeyFromHex(publicKeyHex),
      Buffer.from(manifest.signature, 'base64url'),
    ),
    true,
    'helper release manifest signature is invalid',
  )
  return {
    digest: manifestIdentity.sha256,
    manifest,
    manifestMode: manifestIdentity.mode,
    helperIdentity,
  }
}

function validateSourceSealShape(seal) {
  assert.deepEqual(
    Object.keys(seal).toSorted(),
    API_SOURCE_SEAL_KEYS,
    'API production source seal fields are not canonical',
  )
  assert.equal(seal.schemaVersion, 1)
  assert.equal(seal.protocol, API_SOURCE_SEAL_PROTOCOL)
  assert.equal(seal.version, API_SOURCE_SEAL_VERSION)
  for (const field of [
    'sourceSha256',
    'trackedDiffSha256',
    'serverBinarySha256',
    'helperBinarySha256',
    'helperReleaseManifestSha256',
  ]) {
    assert.match(seal[field], /^sha256:[0-9a-f]{64}$/u, `${field} is invalid`)
  }
  assert.match(seal.gitHead, /^[0-9a-f]{40,64}$/u, 'source seal Git HEAD is invalid')
  assert.match(
    seal.helperReleasePublicKeyHex,
    /^[0-9a-f]{64}$/u,
    'source seal helper public key is invalid',
  )
  assert.equal(seal.serverBinaryPath, 'winwincode-server')
  assert.equal(seal.serverBinaryMode, 0o755)
  assert.equal(seal.helperBinaryPath, HELPER_RELEASE_BINARY_NAME)
  assert.equal(seal.helperBinaryMode, HELPER_RELEASE_BINARY_MODE)
  assert.equal(seal.helperReleaseManifestMode, 0o644)
  assert.equal(seal.helperReleaseManifestPath, HELPER_RELEASE_MANIFEST_NAME)
}

function readSourceSeal(path) {
  const identity = fileIdentity(path, 'API production source seal')
  assert.ok(identity, 'API production source seal identity is required')
  assert.equal(identity.mode & 0o111, 0, 'API production source seal must not be executable')
  assert.equal(identity.mode, 0o644, 'API production source seal must have mode 0644')
  let seal
  try {
    seal = JSON.parse(readFileSync(path, 'utf8'))
  } catch (error) {
    fail(`API production source seal is not valid JSON: ${error.message}`)
  }
  assert.ok(readFileSync(path).byteLength <= API_SOURCE_SEAL_MAX_BYTES, 'API production source seal is too large')
  validateSourceSealShape(seal)
  return { identity, seal }
}

function validateSourceSealFiles({ root, serverBinary, helperExecutable, sealPath, seal }) {
  assert.equal(
    basename(resolve(serverBinary)),
    seal.serverBinaryPath,
    'source seal server path does not match the requested binary',
  )
  assert.equal(
    basename(resolve(helperExecutable)),
    seal.helperBinaryPath,
    'source seal helper path does not match the requested binary',
  )
  const serverIdentity = fileIdentity(serverBinary, 'Server binary')
  const helperIdentity = fileIdentity(helperExecutable, 'kernel helper binary')
  assert.equal(serverIdentity.mode, seal.serverBinaryMode, 'Server binary mode changed')
  assert.equal(helperIdentity.mode, seal.helperBinaryMode, 'kernel helper mode changed')
  assert.equal(serverIdentity.sha256, seal.serverBinarySha256, 'Server binary digest changed')
  assert.equal(helperIdentity.sha256, seal.helperBinarySha256, 'kernel helper digest changed')
  const manifestPath = join(dirname(serverBinary), seal.helperReleaseManifestPath)
  assert.equal(
    resolve(manifestPath),
    resolve(sealPath.replace(API_SOURCE_SEAL_NAME, HELPER_RELEASE_MANIFEST_NAME)),
    'source seal manifest path is not colocated with the Server',
  )
  const manifest = readAndValidateHelperReleaseManifest({
    root,
    helperExecutable,
    manifestPath,
    publicKeyHex: seal.helperReleasePublicKeyHex,
  })
  assert.equal(
    manifest.digest,
    seal.helperReleaseManifestSha256,
    'helper release manifest digest changed',
  )
  assert.equal(
    manifest.manifestMode,
    seal.helperReleaseManifestMode,
    'helper release manifest mode changed',
  )
  const currentSource = sourceTreeIdentity(root)
  assert.equal(
    currentSource.sourceSha256,
    seal.sourceSha256,
    'API production source seal is stale for the current source tree',
  )
  assert.equal(seal.gitHead, currentSource.gitHead, 'API production source seal Git HEAD is stale')
  assert.equal(
    seal.trackedDiffSha256,
    currentSource.trackedDiffSha256,
    'API production source seal tracked diff is stale',
  )
  return {
    path: sealPath,
    seal,
    serverIdentity,
    helperIdentity,
    manifest,
  }
}

/**
 * Emit a source seal after a fresh Server/helper build.  The seal is kept
 * beside the binaries and is never rewritten by a skip-build invocation.
 */
export function writeApiProductionSourceSeal({
  root = ROOT,
  serverBinary,
  helperExecutable,
  helperReleaseManifest,
  expectedSourceIdentity = null,
} = {}) {
  assert.equal(typeof serverBinary, 'string', 'Server binary is required for source sealing')
  assert.equal(typeof helperExecutable, 'string', 'kernel helper is required for source sealing')
  assert.equal(typeof helperReleaseManifest, 'string', 'helper release manifest is required for source sealing')
  const serverIdentity = fileIdentity(serverBinary, 'Server binary')
  const helperIdentity = fileIdentity(helperExecutable, 'kernel helper binary')
  assert.equal(serverIdentity.mode, 0o755, 'Server binary must have mode 0755')
  assert.equal(helperIdentity.mode, HELPER_RELEASE_BINARY_MODE, 'kernel helper must have mode 0755')
  assert.equal(
    dirname(resolve(serverBinary)),
    dirname(resolve(helperExecutable)),
    'Server and kernel helper must be colocated',
  )
  assert.equal(
    resolve(helperReleaseManifest),
    join(dirname(resolve(serverBinary)), HELPER_RELEASE_MANIFEST_NAME),
    'helper release manifest must be colocated with the Server',
  )
  const manifestIdentity = fileIdentity(helperReleaseManifest, 'helper release manifest')
  assert.equal(manifestIdentity.mode, 0o644, 'helper release manifest must have mode 0644')
  const { publicKeyHex } = configuredHelperReleaseKey()
  const manifest = readAndValidateHelperReleaseManifest({
    root,
    helperExecutable,
    manifestPath: helperReleaseManifest,
    publicKeyHex,
  })
  const source = sourceTreeIdentity(root)
  if (expectedSourceIdentity !== null) {
    assert.deepEqual(
      source,
      expectedSourceIdentity,
      'source changed while the API production target was being built',
    )
  }
  const seal = {
    schemaVersion: 1,
    protocol: API_SOURCE_SEAL_PROTOCOL,
    version: API_SOURCE_SEAL_VERSION,
    sourceSha256: source.sourceSha256,
    gitHead: source.gitHead,
    trackedDiffSha256: source.trackedDiffSha256,
    serverBinaryPath: 'winwincode-server',
    serverBinaryMode: serverIdentity.mode,
    serverBinarySha256: serverIdentity.sha256,
    helperBinaryPath: HELPER_RELEASE_BINARY_NAME,
    helperBinaryMode: helperIdentity.mode,
    helperBinarySha256: helperIdentity.sha256,
    helperReleaseManifestMode: manifestIdentity.mode,
    helperReleaseManifestPath: HELPER_RELEASE_MANIFEST_NAME,
    helperReleaseManifestSha256: manifest.digest,
    helperReleasePublicKeyHex: publicKeyHex,
  }
  validateSourceSealShape(seal)
  const sealPath = join(dirname(resolve(serverBinary)), API_SOURCE_SEAL_NAME)
  writeJsonAtomically(sealPath, seal)
  return validateSourceSealFiles({
    root,
    serverBinary,
    helperExecutable,
    sealPath,
    seal,
  })
}

/**
 * Validate a prebuilt target against the current source tree before starting
 * the API.  Missing or stale seals fail closed, so an old target cannot report
 * a contradictory Delivered/released result.
 */
export function verifyApiProductionSourceSeal({
  root = ROOT,
  serverBinary,
  helperExecutable,
} = {}) {
  assert.equal(typeof serverBinary, 'string', 'Server binary is required for source verification')
  assert.equal(typeof helperExecutable, 'string', 'kernel helper is required for source verification')
  const sealPath = join(dirname(resolve(serverBinary)), API_SOURCE_SEAL_NAME)
  let sourceSeal
  try {
    sourceSeal = readSourceSeal(sealPath)
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error)
    fail(`API production target rejected: source seal missing or invalid (${detail})`)
  }
  return validateSourceSealFiles({
    root,
    serverBinary,
    helperExecutable,
    sealPath,
    seal: sourceSeal.seal,
  })
}

export function writeHelperReleaseManifest(root, helperExecutable) {
  const { privateKey } = configuredHelperReleaseKey()
  const sourceSha256 = `sha256:${createHash('sha256')
    .update(readFileSync(join(root, 'crates', 'helper', 'src', 'main.rs')))
    .digest('hex')}`
  const fields = {
    schemaVersion: 1,
    protocol: 'winwincode-kernel-helper-release',
    version: 1,
    packageVersion: '0.0.0',
    sourceSha256,
    binarySha256: `sha256:${createHash('sha256').update(readFileSync(helperExecutable)).digest('hex')}`,
    binaryPath: HELPER_RELEASE_BINARY_NAME,
    binaryMode: HELPER_RELEASE_BINARY_MODE,
  }
  const signature = sign(null, helperReleaseSigningBytes(fields), privateKey)
    .toString('base64url')
  const manifest = {
    ...fields,
    signature,
  }
  const expectedPublicKey = process.env.WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX
    ?? helperReleasePublicKeyHex(privateKey)
  assert.equal(
    helperReleasePublicKeyHex(privateKey),
    expectedPublicKey,
    'helper release manifest signer must match the compiled release public key',
  )
  const manifestPath = join(dirname(helperExecutable), HELPER_RELEASE_MANIFEST_NAME)
  writeJsonAtomically(manifestPath, manifest)
  return manifestPath
}

function commandRequest(requestId, command, expectedRevision, payload) {
  return {
    schemaVersion: SCHEMA_VERSION,
    requestId,
    actor: ACTOR,
    scope: SCOPE,
    command,
    expectedRevision,
    payload,
  }
}

function queryRequest(requestId, query, parameters) {
  return {
    schemaVersion: SCHEMA_VERSION,
    requestId,
    actor: ACTOR,
    scope: SCOPE,
    query,
    parameters,
    page: page(),
  }
}

function responseSetCookie(headers) {
  const values = headers['set-cookie']
  if (!Array.isArray(values) || values.length === 0) return null
  const [cookie] = values[0].split(';', 1)
  return cookie?.length > 0 ? cookie : null
}

/**
 * Request JSON without a UI process or global TLS switches. The fixture uses a
 * self-signed certificate, so only this local request opts out of trust.
 */
function requestJson(url, {
  method = 'GET',
  origin,
  cookie = null,
  authorization = null,
  body = undefined,
  timeoutMillis = 30_000,
} = {}) {
  const target = new URL(url)
  const serialized = body === undefined ? null : JSON.stringify(body)
  const headers = {
    Accept: 'application/json',
    Connection: 'close',
    Origin: origin,
    ...(serialized === null ? {} : {
      'Content-Type': 'application/json',
      'Content-Length': Buffer.byteLength(serialized),
    }),
    ...(cookie === null ? {} : { Cookie: cookie }),
    ...(authorization === null ? {} : { Authorization: `Bearer ${authorization}` }),
  }
  return new Promise((resolvePromise, reject) => {
    const request = httpsRequest(target, {
      method,
      headers,
      rejectUnauthorized: false,
      servername: 'control.localhost',
      timeout: timeoutMillis,
    }, response => {
      const chunks = []
      response.setEncoding('utf8')
      response.on('data', chunk => chunks.push(chunk))
      response.on('end', () => {
        const text = chunks.join('')
        let json = null
        if (text.length > 0) {
          try {
            json = JSON.parse(text)
          } catch {
            reject(new Error(`HTTP ${response.statusCode ?? 0} returned invalid JSON`))
            return
          }
        }
        resolvePromise({
          headers: response.headers,
          json,
          status: response.statusCode ?? 0,
          text,
        })
      })
    })
    request.on('timeout', () => request.destroy(new Error('HTTP request timed out')))
    request.on('error', reject)
    if (serialized !== null) request.write(serialized)
    request.end()
  })
}

class ApiClient {
  constructor(baseUrl, origin) {
    this.baseUrl = baseUrl.endsWith('/') ? baseUrl.slice(0, -1) : baseUrl
    this.origin = origin
    this.cookie = null
    this.nextRequest = 1
    this.actor = ACTOR
    this.scope = SCOPE
    this.session = null
  }

  requestId() {
    const value = id('req', this.nextRequest)
    this.nextRequest += 1
    return value
  }

  async bootstrap(proof) {
    const response = await requestJson(`${this.baseUrl}/api/v1/auth/session`, {
      method: 'POST',
      origin: this.origin,
      authorization: proof,
      body: { schemaVersion: SCHEMA_VERSION },
    })
    assert.equal(response.status, 201, 'API authentication bootstrap must return 201')
    this.cookie = responseSetCookie(response.headers)
    assert.notEqual(this.cookie, null, 'API authentication must issue a session cookie')
    assert.equal(response.json?.schemaVersion, SCHEMA_VERSION)
    assert.deepEqual(response.json?.actor, this.actor)
    this.session = response.json
    return response.json
  }

  async command(command, expectedRevision, payload, requestId = undefined) {
    const request = commandRequest(
      requestId ?? this.requestId(),
      command,
      expectedRevision,
      payload,
    )
    const response = await requestJson(`${this.baseUrl}/api/v1/commands`, {
      method: 'POST',
      origin: this.origin,
      cookie: this.cookie,
      body: request,
    })
    if (response.status < 200 || response.status >= 300) {
      const code = response.json?.error?.code ?? 'HTTP_ERROR'
      const message = response.json?.error?.message
      const error = new Error(`${command} returned HTTP ${response.status} (${code})${message === undefined ? '' : `: ${message}`}`)
      error.code = code
      error.status = response.status
      throw error
    }
    assert.equal(response.json?.requestId, request.requestId, `${command} request correlation`)
    assert.equal(response.json?.command, command, `${command} command correlation`)
    assert.equal(response.json?.schemaVersion, SCHEMA_VERSION, `${command} schema`)
    return response.json
  }

  async query(query, parameters) {
    const request = queryRequest(this.requestId(), query, parameters)
    const response = await requestJson(`${this.baseUrl}/api/v1/queries`, {
      method: 'POST',
      origin: this.origin,
      cookie: this.cookie,
      body: request,
    })
    if (response.status < 200 || response.status >= 300) {
      const code = response.json?.error?.code ?? 'HTTP_ERROR'
      const error = new Error(`${query} returned HTTP ${response.status} (${code})`)
      error.code = code
      error.status = response.status
      throw error
    }
    assert.equal(response.json?.requestId, request.requestId, `${query} request correlation`)
    assert.equal(response.json?.query, query, `${query} query correlation`)
    assert.equal(response.json?.schemaVersion, SCHEMA_VERSION, `${query} schema`)
    return response.json
  }
}

function assertCompleted(response, command, previousRevision = undefined) {
  assert.equal(response.outcome, 'completed', `${command} must complete through the API`)
  assert.equal(response.command, command)
  assert.equal(Number.isInteger(response.currentRevision), true)
  assert.equal(Number.isInteger(response.previousRevision), true)
  if (previousRevision !== undefined) {
    assert.equal(response.previousRevision, previousRevision, `${command} previous revision`)
    assert.ok(response.currentRevision > previousRevision, `${command} must advance its revision`)
  }
}

function assertRuntimeSession(session, { productSessionId, stageRunId = undefined } = {}) {
  assert.equal(typeof session.executionJobId, 'string')
  assert.equal(typeof session.workerSessionId, 'string')
  assert.equal(typeof session.codexThreadId, 'string')
  assert.equal(session.productSessionId, productSessionId)
  assert.ok(Number.isInteger(session.asOfSequence) && session.asOfSequence > 0)
  if (stageRunId !== undefined) assert.equal(session.stageRunId, stageRunId)
}

function stageBindingReady(stage) {
  const binding = stage.sessionBinding
  if (stage.actorType !== 'codex' || binding === null || typeof binding !== 'object') return false
  return typeof binding.executionJobId === 'string'
    && typeof binding.workerId === 'string'
    && typeof binding.workerSessionId === 'string'
    && typeof binding.codexThreadId === 'string'
    && binding.sessionIdentity?.productSessionId === binding.productSessionId
    && binding.sessionIdentity?.workerSessionId === binding.workerSessionId
    && binding.sessionIdentity?.codexThreadId === binding.codexThreadId
    && binding.sessionIdentity?.stageRunId === stage.id
}

async function waitFor(predicate, label, timeoutMillis) {
  const deadline = Date.now() + timeoutMillis
  for (;;) {
    const value = await predicate()
    if (value) return value
    if (Date.now() >= deadline) fail(`timed out waiting for ${label}`)
    await new Promise(resolvePromise => setTimeout(resolvePromise, POLL_INTERVAL_MILLIS))
  }
}

async function freePort() {
  const server = createNetServer()
  await new Promise((resolvePromise, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', resolvePromise)
  })
  const address = server.address()
  const port = typeof address === 'object' && address !== null ? address.port : 0
  await new Promise(resolvePromise => server.close(resolvePromise))
  assert.ok(port > 0, 'free port helper must return a port')
  return port
}

function createCertificate(directory) {
  const configuration = join(directory, 'openssl.cnf')
  const key = join(directory, 'fixture-key.pem')
  const cert = join(directory, 'fixture-cert.pem')
  writeFileSync(configuration, `[req]
distinguished_name = dn
x509_extensions = extensions
prompt = no
[dn]
CN = control.localhost
[extensions]
subjectAltName = @names
[names]
DNS.1 = control.localhost
DNS.2 = api.localhost
`)
  const result = spawnSync('openssl', [
    'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-sha256', '-days', '1',
    '-config', configuration, '-keyout', key, '-out', cert,
  ], { cwd: ROOT, encoding: 'utf8', stdio: 'pipe' })
  assert.equal(result.status, 0, `openssl certificate generation failed: ${result.stderr}`)
  return { cert, key }
}

function sourceRevision() {
  const result = spawnSync('git', ['rev-parse', 'HEAD'], { cwd: ROOT, encoding: 'utf8' })
  assert.equal(result.status, 0, result.stderr)
  return result.stdout.trim()
}

export function prepareControlledRepository({ fixtureDirectory }) {
  const sourceRoot = resolve(fixtureDirectory, 'source-repositories')
  const repository = join(sourceRoot, IDS.repository)
  mkdirSync(sourceRoot, { recursive: true })
  const init = spawnSync(
    'git',
    ['init', '--quiet', '--initial-branch=main', repository],
    { cwd: sourceRoot, encoding: 'utf8', stdio: 'pipe' },
  )
  assert.equal(init.status, 0, `controlled API fixture repository creation failed: ${init.stderr}`)
  writeFileSync(
    join(repository, 'package.json'),
    `${JSON.stringify({
      name: 'winwincode-api-fixture',
      private: true,
      scripts: { verify: "printf '%s\\n' 'fixture verified'" },
    }, null, 2)}\n`,
  )
  writeFileSync(join(repository, 'pnpm-lock.yaml'), "lockfileVersion: '9.0'\n")
  writeFileSync(
    join(repository, '.winwincode-api-candidate'),
    'deterministic StrongFlow candidate baseline\n',
  )
  const environment = {
    ...process.env,
    GIT_CONFIG_NOSYSTEM: '1',
    GIT_AUTHOR_NAME: 'WinWinCode API fixture',
    GIT_AUTHOR_EMAIL: 'api-fixture@winwincode.invalid',
    GIT_COMMITTER_NAME: 'WinWinCode API fixture',
    GIT_COMMITTER_EMAIL: 'api-fixture@winwincode.invalid',
    GIT_AUTHOR_DATE: '2000-01-01T00:00:00Z',
    GIT_COMMITTER_DATE: '2000-01-01T00:00:00Z',
  }
  for (const args of [
    ['add', '--all'],
    ['commit', '--quiet', '--message=WinWinCode API fixture baseline'],
  ]) {
    const result = spawnSync('git', args, {
      cwd: repository,
      env: environment,
      encoding: 'utf8',
      stdio: 'pipe',
    })
    assert.equal(
      result.status,
      0,
      `controlled API fixture repository creation failed: ${result.stderr}`,
    )
  }
  const revision = spawnSync('git', ['rev-parse', 'HEAD'], {
    cwd: repository,
    env: environment,
    encoding: 'utf8',
    stdio: 'pipe',
  })
  assert.equal(
    revision.status,
    0,
    `controlled API fixture revision lookup failed: ${revision.stderr}`,
  )
  return { sourceRoot, repository, revision: revision.stdout.trim() }
}

function removeControlledRepository({ repository }) {
  rmSync(repository, { recursive: true, force: true })
}

export function serverTargetDirectory(root) {
  const configured = process.env.CARGO_TARGET_DIR
  return configured === undefined || configured.length === 0
    ? resolve(root, 'target')
    : resolve(root, configured)
}

function redactedOutput(output, proof) {
  return output.replaceAll(proof, '[redacted]')
}

function spawnStandaloneServer({
  certificate,
  directory,
  origin,
  port,
  proof,
  root,
  checkoutRevision,
  helperExecutable,
  helperReleaseManifest,
  repositoryRoot,
  sourceRoot,
  serverEnvironment = {},
  serverBinary,
}) {
  const controlUrl = `https://127.0.0.1:${String(port)}`
  const errors = []
  const binary = serverBinary ?? resolve(serverTargetDirectory(root), 'debug/winwincode-server')
  const child = spawn(binary, [], {
    cwd: root,
    detached: process.platform !== 'win32',
    env: {
      ...process.env,
      ...serverEnvironment,
      WWC_SERVER_BIND: `127.0.0.1:${String(port)}`,
      WWC_SERVER_PUBLIC_URL: `https://control.localhost:${String(port)}`,
      WWC_SERVER_DATA_DIRECTORY: resolve(directory, 'server-data'),
      WWC_SERVER_ALLOWED_ORIGINS: origin,
      WWC_SERVER_BOOTSTRAP_PROOF: proof,
      WWC_SERVER_AUTH_SUBJECT: IDS.actor,
      WWC_SERVER_REPOSITORY_ROOT: repositoryRoot,
      WWC_SERVER_SOURCE_ROOT: sourceRoot,
      WWC_SERVER_CHECKOUT_REVISION: checkoutRevision,
      WWC_SERVER_HELPER_EXECUTABLE: helperExecutable,
      WWC_SERVER_HELPER_RELEASE_MANIFEST: helperReleaseManifest,
      WWC_SERVER_ACTION_SIGNING_KEY_HEX: '1f'.repeat(32),
      WWC_SERVER_EXECUTION_ENVELOPE_DIGEST: `sha256:${'a'.repeat(64)}`,
      WWC_SERVER_MODEL_CREDENTIAL_REFERENCE_ID: IDS.credential,
      WWC_SERVER_ORGANIZATION_ID: IDS.organization,
      WWC_SERVER_WORKSPACE_ID: IDS.workspace,
      WWC_SERVER_PROJECT_ID: IDS.project,
      WWC_SERVER_REPOSITORY_ID: IDS.repository,
      GITHUB_REPOSITORY: 'winwincode/api-fixture',
      GITHUB_CREDENTIAL_REFERENCE_ID: IDS.credential,
      GITHUB_API_BASE_URL: 'https://api.github.example',
      SECRET_DIRECTORY: resolve(directory, 'publication-secrets'),
      PUBLICATION_REQUESTERS: IDS.actor,
      PUBLICATION_APPROVERS: IDS.actor,
      PUBLICATION_APPROVAL_MAX_AGE_MILLIS: '86400000',
      WWC_SERVER_TLS_CERTIFICATE: certificate.cert,
      WWC_SERVER_TLS_PRIVATE_KEY: certificate.key,
      GIT_CONFIG_NOSYSTEM: '1',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  child.stdout.setEncoding('utf8')
  child.stderr.setEncoding('utf8')
  // Allow the CLI runner to capture the same runtime diagnostics as callers
  // using runApiProductionVertical({ serverEnvironment }). The explicit
  // option still wins when a test needs a per-run log path.
  const debugLog = serverEnvironment.WWC_DEBUG_RUNTIME_LOG
    ?? process.env.WWC_DEBUG_RUNTIME_LOG
  child.stdout.on('data', chunk => {
    errors.push(chunk)
    if (debugLog !== undefined) appendFileSync(debugLog, chunk)
  })
  child.stderr.on('data', chunk => {
    errors.push(chunk)
    if (debugLog !== undefined) appendFileSync(debugLog, chunk)
  })
  return { child, controlUrl, errors }
}

async function waitForServer(baseUrl, origin, child, errors, proof, timeoutMillis) {
  return waitFor(async () => {
    if (child.exitCode !== null) {
      fail(`standalone Server exited before health: ${redactedOutput(errors.join(''), proof)}`)
    }
    try {
      const response = await requestJson(`${baseUrl}/health`, {
        origin,
        timeoutMillis: 2_000,
      })
      return response.status === 200 && response.json?.status === 'ready'
        ? response.json
        : false
    } catch {
      return false
    }
  }, 'standalone Server health', Math.min(timeoutMillis, 30_000))
}

async function stopServer(child) {
  if (child.exitCode !== null || child.signalCode !== null) return
  const exited = new Promise(resolvePromise => child.once('exit', resolvePromise))
  // Let the Server own shutdown of its supervised Worker. Sending SIGINT to
  // the whole detached process group also interrupts the helper before the
  // Server can release its durable lease, making an immediate restart look
  // like a stale-instance failure.
  child.kill('SIGINT')
  const graceful = await Promise.race([
    exited.then(() => true),
    new Promise(resolvePromise => setTimeout(
      () => resolvePromise(false),
      SERVER_GRACEFUL_STOP_TIMEOUT_MILLIS,
    )),
  ])
  if (graceful) return
  if (process.platform !== 'win32' && child.pid !== undefined) {
    try {
      process.kill(-child.pid, 'SIGKILL')
    } catch {
      child.kill('SIGKILL')
    }
  } else {
    child.kill('SIGKILL')
  }
  await Promise.race([
    exited,
    new Promise(resolvePromise => setTimeout(resolvePromise, 5_000)),
  ])
  assert.notEqual(child.exitCode, null, 'standalone Server process must stop')
}

async function startServer({
  root,
  certificate,
  directory,
  proof,
  serverBinary,
  checkoutRevision,
  helperExecutable,
  helperReleaseManifest,
  repositoryRoot,
  sourceRoot,
  serverEnvironment,
  timeoutMillis,
}) {
  const port = await freePort()
  const origin = `https://api.localhost:${String(port)}`
  const started = spawnStandaloneServer({
    certificate,
    directory,
    origin,
    port,
    proof,
    root,
    checkoutRevision,
    helperExecutable,
    helperReleaseManifest,
    repositoryRoot,
    sourceRoot,
    serverEnvironment,
    serverBinary,
  })
  try {
    const health = await waitForServer(
      started.controlUrl,
      origin,
      started.child,
      started.errors,
      proof,
      timeoutMillis,
    )
    started.health = health
  } catch (error) {
    await stopServer(started.child).catch(() => {})
    throw error
  }
  return { ...started, origin }
}

async function runChat(
  client,
  timeoutMillis,
  {
    create = true,
    modelRoute = configuredModelRoute(),
    productSessionId = IDS.session,
  } = {},
) {
  if (create) {
    const created = await client.command('session.create', 0, {
      productSessionId,
      projectId: IDS.project,
      repositoryId: IDS.repository,
      title: 'API production Chat',
      modelRoute,
    })
    assertCompleted(created, 'session.create', 0)
    assert.equal(created.result.id, productSessionId)
  }
  const submitted = await client.command('chat.submit', 1, {
    productSessionId,
    message: 'Run the deterministic local API workflow.',
  })
  assertCompleted(submitted, 'chat.submit', 1)
  assert.equal(submitted.result.id, productSessionId)
  const terminal = await waitFor(async () => {
    const response = await client.query('session.messages.list', { productSessionId })
    const assistant = response.result?.items?.find(message => (
      message.productSessionId === productSessionId
      && message.role === 'assistant'
      && message.state === 'completed'
      && typeof message.content === 'string'
      && message.content.trim().length > 0
    ))
    return assistant === undefined ? false : { response, assistant }
  }, `Chat ${productSessionId} terminal projection`, timeoutMillis)
  const runtime = await waitFor(async () => {
    const response = await client.query('runtime.projection.get', {
      kind: 'product-session',
      productSessionId,
    })
    const sessions = response.result?.sessions
    if (!Array.isArray(sessions) || sessions.length === 0) return false
    const matching = sessions.find(session => session.productSessionId === productSessionId)
    return matching === undefined ? false : { response, session: matching }
  }, `Chat ${productSessionId} runtime projection`, timeoutMillis)
  assertRuntimeSession(runtime.session, { productSessionId })
  return {
    assistant: terminal.assistant,
    messages: terminal.response.result.items,
    runtime: runtime.session,
    submitted,
  }
}

async function runCancelledSession(client, modelRoute = configuredModelRoute()) {
  const created = await client.command('session.create', 0, {
    productSessionId: IDS.cancelSession,
    projectId: IDS.project,
    repositoryId: IDS.repository,
    title: 'API production cancellation',
    modelRoute,
  })
  assertCompleted(created, 'session.create', 0)
  assert.equal(created.result.id, IDS.cancelSession)
  const cancelled = await client.command('session.cancel', 1, {
    productSessionId: IDS.cancelSession,
    reason: 'API production cancellation evidence',
  })
  assertCompleted(cancelled, 'session.cancel', 1)
  assert.equal(cancelled.result.id, IDS.cancelSession)
  assert.equal(cancelled.result.state, 'cancelled')
  const reloaded = (await client.query('session.get', {
    productSessionId: IDS.cancelSession,
  })).result
  assert.equal(reloaded.id, IDS.cancelSession)
  assert.equal(reloaded.state, 'cancelled')
  assert.equal(reloaded.revision, cancelled.result.revision)
  return {
    providerRoute: modelRoute,
    revision: reloaded.revision,
    state: reloaded.state,
  }
}

function deliverySpec(baseRevision) {
  return {
    acceptanceCriteria: [{
      id: 'api-terminal-criterion',
      required: true,
      title: 'The API production workflow reaches a terminal projection',
    }],
    baseRevision,
    goal: 'Verify Chat and StrongFlow through the canonical local API',
    publicationTarget: null,
    repositoryId: IDS.repository,
    title: 'API production StrongFlow',
  }
}

function approveSolutionResolution(detail) {
  const review = detail.solutionReview
  assert.notEqual(review, null, 'solution review must be present before approval')
  return JSON.stringify({
    schemaVersion: 1,
    protocol: 'winwincode.solution-review-decision.v1',
    deliveryId: detail.deliveryId,
    deliverySpecId: review.deliverySpecId,
    deliverySpecRevision: review.deliverySpecRevision,
    reviewStageRunId: review.reviewStageRunId,
    attentionItemId: review.attentionItemId,
    reviewSetSha256: review.reviewSetSha256.replace(/^sha256:/, ''),
    action: 'approve',
    comments: 'Approve the current bounded solution review through the API.',
    requestedChanges: null,
  })
}

function candidateArtifactSummary(candidate) {
  if (candidate === null || candidate === undefined) return null
  return {
    candidateRef: candidate.candidateRef ?? null,
    candidateCommitId: candidate.candidateCommitId ?? null,
    candidateTreeId: candidate.candidateTreeId ?? null,
    diffSha256: candidate.diffSha256 ?? null,
    producerStageRunId: candidate.producerStageRunId ?? null,
    producerSessionBindingId: candidate.producerSessionBindingId ?? null,
    frozenAt: candidate.frozenAt ?? null,
  }
}

function stageRunSummary(stage, modelRoute) {
  return {
    id: stage.id,
    role: stage.role,
    status: stage.status,
    attempt: stage.attempt,
    executionJobId: stage.sessionBinding?.executionJobId ?? null,
    providerRoute: modelRoute,
  }
}

function deliveryFailureSummary(detail, modelRoute = configuredModelRoute()) {
  return {
    deliveryRevision: detail?.deliveryRevision ?? null,
    status: detail?.status ?? null,
    stages: Array.isArray(detail?.stages)
      ? detail.stages.map(stage => stageRunSummary(stage, modelRoute))
      : [],
    currentCandidate: detail?.currentCandidate === null
      ? null
      : detail?.currentCandidate === undefined
        ? null
        : {
          diffSha256: detail.currentCandidate.diffSha256 ?? null,
          status: detail.currentCandidate.status ?? null,
        },
    candidateArtifact: candidateArtifactSummary(detail?.currentCandidate),
    verdictStatus: detail?.verdict?.status ?? null,
    taskStatuses: Array.isArray(detail?.tasks)
      ? detail.tasks.map(task => task.status)
      : [],
    openAttentionIds: Array.isArray(detail?.attention)
      ? detail.attention
        .filter(item => item.status === 'open')
        .map(item => item.id)
      : [],
  }
}

function transientDeliveryErrorSummary(errors) {
  const counts = new Map()
  for (const error of errors) {
    const key = `${error.command}:${error.code}`
    const previous = counts.get(key)
    if (previous === undefined) {
      counts.set(key, {
        command: error.command,
        code: error.code,
        status: error.status,
        count: 1,
        lastMessage: error.message,
      })
    } else {
      previous.count += 1
      previous.lastMessage = error.message
    }
  }
  return [...counts.values()]
}

async function driveDelivery(client, timeoutMillis, modelRoute = configuredModelRoute()) {
  const observations = []
  const actions = []
  const transientErrors = []
  const deadline = Date.now() + timeoutMillis
  let detail = null
  let terminalCommand = null
  for (;;) {
    detail = (await client.query('delivery.get', { deliveryId: IDS.delivery })).result
    const observation = { revision: detail.deliveryRevision, status: detail.status }
    const previous = observations.at(-1)
    if (previous?.revision !== observation.revision || previous?.status !== observation.status) {
      assert.ok(observations.length < MAX_DELIVERY_TRANSITIONS, 'StrongFlow transition trace is bounded')
      observations.push(observation)
    }
    if (detail.status === 'delivered') break
    if (Date.now() >= deadline) {
      fail(`StrongFlow did not reach delivered: ${JSON.stringify({
        observations,
        failure: deliveryFailureSummary(detail, modelRoute),
        transientErrors: transientDeliveryErrorSummary(transientErrors),
      })}`)
    }

    let command = null
    let payload = null
    if (detail.solutionReview?.reviewStatus === 'pending') {
      command = 'delivery.resolve_attention'
      payload = {
        deliveryId: IDS.delivery,
        attentionItemId: detail.solutionReview.attentionItemId,
        decision: 'resolve',
        resolution: approveSolutionResolution(detail),
        remediation: null,
      }
    } else if (detail.solutionReview?.reviewStatus === 'approved' && detail.tasks.length === 0) {
      command = 'delivery.approve_task_breakdown'
      payload = {
        deliveryId: IDS.delivery,
        reviewSetSha256: detail.solutionReview.reviewSetSha256,
      }
    } else if (
      detail.currentCandidate !== null
      && detail.verdict === null
      && !detail.stages.some(stage => ['running', 'waiting'].includes(stage.status))
    ) {
      command = 'delivery.submit_verdict'
      const candidateRef = detail.currentCandidate.candidateRef
      assert.match(
        candidateRef ?? '',
        /^git-candidate:sha256:[0-9a-f]{64}$/u,
        'Delivered candidate must expose its canonical Git candidate reference',
      )
      payload = {
        deliveryId: IDS.delivery,
        // `candidateDigest` is the stale-check suffix of the immutable
        // candidate reference.  `diffSha256` identifies the patch bytes and
        // is deliberately a different value.
        candidateDigest: candidateRef.slice('git-candidate:'.length),
      }
    } else {
      const attention = detail.attention.find(item => item.status === 'open')
      if (attention !== undefined) {
        command = 'delivery.resolve_attention'
        payload = {
          deliveryId: IDS.delivery,
          attentionItemId: attention.id,
          decision: 'resolve',
          resolution: 'Resolve the bounded API workflow attention item.',
          remediation: null,
        }
      } else {
        command = 'delivery.advance'
        payload = { deliveryId: IDS.delivery }
      }
    }

    const beforeRevision = detail.deliveryRevision
    const requestId = client.requestId()
    const submittedCommand = {
      command,
      expectedRevision: beforeRevision,
      payload,
      requestId,
    }
    let response
    try {
      response = await client.command(command, beforeRevision, payload, requestId)
    } catch (error) {
      if (error?.code === 'REVISION_CONFLICT' || error?.code === 'WRONG_STATE') continue
      if (error?.code === 'TRUSTED_FACTS_UNAVAILABLE') {
        if (transientErrors.length < 128) {
          transientErrors.push({
            command,
            code: error.code,
            status: error.status ?? null,
            message: error.message,
          })
        }
        await new Promise(resolvePromise => setTimeout(resolvePromise, POLL_INTERVAL_MILLIS))
        continue
      }
      let failureDetail = detail
      try {
        failureDetail = (await client.query('delivery.get', {
          deliveryId: IDS.delivery,
        })).result ?? detail
      } catch {
        // Preserve the command error when the diagnostic query is unavailable.
      }
      const summary = JSON.stringify(deliveryFailureSummary(failureDetail, modelRoute))
      if (error instanceof Error) error.message = `${error.message}; delivery=${summary}`
      throw error
    }
    assertCompleted(response, command, beforeRevision)
    actions.push({
      command,
      fromRevision: beforeRevision,
      toRevision: response.currentRevision,
    })
    terminalCommand = submittedCommand
    detail = null
  }

  const terminal = detail
  assert.equal(terminal.status, 'delivered')
  assert.notEqual(terminal.currentCandidate, null, 'Delivered projection must contain a frozen candidate')
  assert.equal(terminal.verdict?.status, 'pass', 'Delivered projection must contain a passing verdict')
  assert.equal(terminal.attention.filter(item => item.status === 'open').length, 0)
  assert.ok(terminal.evidence.length > 0, 'Delivered projection must expose canonical evidence')
  assert.ok(terminal.stages.length > 0, 'Delivered projection must expose canonical stages')
  for (const role of ['planner', 'executor', 'reviewer', 'verifier']) {
    const stage = terminal.stages.find(candidate => (
      candidate.role.toLowerCase() === role
      && candidate.status === 'succeeded'
      && stageBindingReady(candidate)
    ))
    assert.notEqual(stage, undefined, `missing succeeded bound ${role} StageRun`)
  }
  assert.ok(terminal.tasks.length > 0, 'Delivered projection must contain promoted tasks')
  assert.equal(terminal.tasks.every(task => task.status === 'completed'), true)
  assert.equal(terminal.verdict.criteria.every(criterion => criterion.verdict === 'pass'), true)
  return { actions, detail: terminal, observations, terminalCommand }
}

async function commandEventually(client, command, expectedRevision, payload, timeoutMillis) {
  const deadline = Date.now() + timeoutMillis
  for (;;) {
    try {
      return await client.command(command, expectedRevision, payload)
    } catch (error) {
      if (error?.code !== 'TRUSTED_FACTS_UNAVAILABLE') throw error
      if (Date.now() >= deadline) throw error
      await new Promise(resolvePromise => setTimeout(resolvePromise, POLL_INTERVAL_MILLIS))
    }
  }
}

async function runtimeStageEvidence(client, detail) {
  const stageRuns = detail.stages.filter(stage => stageBindingReady(stage))
  const snapshots = []
  for (const stage of stageRuns) {
    const binding = stage.sessionBinding
    const response = await client.query('runtime.projection.get', {
      kind: 'delivery-stage',
      deliveryId: IDS.delivery,
      productSessionId: binding.productSessionId,
      stageRunId: stage.id,
      atCursor: detail.readCursor,
    })
    const sessions = response.result?.sessions
    assert.ok(Array.isArray(sessions), `${stage.role} runtime sessions must be an array`)
    const session = sessions.find(candidate => candidate.stageRunId === stage.id)
    assert.notEqual(session, undefined, `${stage.role} runtime session must be present`)
    assertRuntimeSession(session, {
      productSessionId: binding.productSessionId,
      stageRunId: stage.id,
    })
    snapshots.push({
      role: stage.role.toLowerCase(),
      stageRunId: stage.id,
      session,
    })
  }
  return snapshots
}

/**
 * Start one standalone Server and prove the API-only Chat + StrongFlow path.
 * The returned report contains only canonical, secret-free observations.
 */
export async function runApiProductionVertical({
  build = process.env.WWC_API_SKIP_BUILD !== '1',
  directory = null,
  root = ROOT,
  serverBinary = null,
  restart = true,
  repeat = true,
  includeStrongFlow = true,
  serverEnvironment = {},
  timeoutMillis = DEFAULT_TIMEOUT_MILLIS,
} = {}) {
  const modelRoute = configuredModelRoute(serverEnvironment)
  const binary = serverBinary ?? resolve(serverTargetDirectory(root), 'debug/winwincode-server')
  const buildSourceIdentity = build ? sourceTreeIdentity(root) : null
  let helperReleaseManifest
  let sourceSeal
  if (build) {
    const helperReleaseKey = configuredHelperReleaseKey()
    const buildEnvironment = {
      ...process.env,
      WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX: helperReleaseKey.publicKeyHex,
    }
    const result = spawnSync('cargo', [
      'build', '-p', 'winwincode-server', '--bin', 'winwincode-server',
      '--locked', '--offline',
    ], { cwd: root, encoding: 'utf8', env: buildEnvironment, stdio: 'inherit' })
    assert.equal(result.status, 0, 'winwincode-server production binary build failed')
    // The production adapter intentionally rejects oversized debug helpers.
    // Build the compact release helper, then co-locate it with the debug
    // Server so the runner exercises the same signed helper boundary as the
    // packaged application without creating a second target directory.
    const helperResult = spawnSync('cargo', [
      'build', '--release', '-p', 'winwincode-kernel-helper', '--locked', '--offline',
    ], { cwd: root, encoding: 'utf8', env: buildEnvironment, stdio: 'inherit' })
    assert.equal(helperResult.status, 0, 'winwincode-kernel-helper release build failed')
    const releaseHelper = resolve(serverTargetDirectory(root), 'release/winwincode-kernel-helper')
    const colocatedHelper = resolve(dirname(binary), 'winwincode-kernel-helper')
    assert.equal(existsSync(releaseHelper), true, `release helper is missing: ${releaseHelper}`)
    if (releaseHelper !== colocatedHelper) copyFileSync(releaseHelper, colocatedHelper)
    chmodSync(colocatedHelper, 0o755)
  }
  assert.equal(existsSync(binary), true, `server binary is missing: ${binary}`)
  // Keep the helper and Server in one executable directory.  The production
  // adapter authenticates that both paths share the running executable's
  // directory; deriving the helper from CARGO_TARGET_DIR would silently pick
  // another target when callers provide --server-binary.
  const helperExecutable = resolve(dirname(binary), 'winwincode-kernel-helper')
  assert.equal(existsSync(helperExecutable), true, `kernel helper binary is missing: ${helperExecutable}`)
  if (build) {
    helperReleaseManifest = writeHelperReleaseManifest(root, helperExecutable)
    sourceSeal = writeApiProductionSourceSeal({
      root,
      serverBinary: binary,
      helperExecutable,
      helperReleaseManifest,
      expectedSourceIdentity: buildSourceIdentity,
    })
  } else {
    sourceSeal = verifyApiProductionSourceSeal({
      root,
      serverBinary: binary,
      helperExecutable,
    })
    helperReleaseManifest = join(dirname(binary), HELPER_RELEASE_MANIFEST_NAME)
  }
  const ownedDirectory = directory === null
  const fixtureDirectory = directory ?? mkdtempSync(join(tmpdir(), 'winwincode-api-production-'))
  mkdirSync(fixtureDirectory, { recursive: true })
  const certificate = createCertificate(fixtureDirectory)
  const proof = randomBytes(32).toString('base64url')
  const proofs = [proof]
  const controlledRepository = prepareControlledRepository({
    fixtureDirectory,
  })
  const baseline = controlledRepository.revision
  let started = null
  let api = null
  let serverOutput = ''
  let failure = null
  const report = {
    schemaVersion: 'winwincode.api-production-vertical.v1',
    artifacts: {
      sourceSeal: {
        gitHead: sourceSeal.seal.gitHead,
        helperBinarySha256: sourceSeal.seal.helperBinarySha256,
        helperReleaseManifestSha256: sourceSeal.seal.helperReleaseManifestSha256,
        serverBinarySha256: sourceSeal.seal.serverBinarySha256,
        sourceSha256: sourceSeal.seal.sourceSha256,
        trackedDiffSha256: sourceSeal.seal.trackedDiffSha256,
      },
    },
    flow: {},
    health: { initial: null, afterRestart: null },
    restart: null,
    deterministic: null,
  }

  try {
    started = await startServer({
      root,
      certificate,
      directory: fixtureDirectory,
      proof,
      checkoutRevision: baseline,
      helperExecutable,
      helperReleaseManifest,
      repositoryRoot: controlledRepository.repository,
      sourceRoot: controlledRepository.sourceRoot,
      serverEnvironment,
      serverBinary: binary,
      timeoutMillis,
    })
    report.health.initial = started.health.status
    api = new ApiClient(started.controlUrl, started.origin)
    await api.bootstrap(proof)

    const chat = await runChat(api, timeoutMillis, { modelRoute })
    report.flow.chat = {
      assistant: {
        content: chat.assistant.content,
        role: chat.assistant.role,
        state: chat.assistant.state,
      },
      messageCount: chat.messages.length,
      runtime: {
        asOfSequence: chat.runtime.asOfSequence,
        codexThreadId: chat.runtime.codexThreadId,
        executionJobId: chat.runtime.executionJobId,
        workerSessionId: chat.runtime.workerSessionId,
      },
      providerRoute: modelRoute,
      status: 'Completed',
    }

    if (repeat) {
      const repeated = await runChat(api, timeoutMillis, {
        modelRoute,
        productSessionId: IDS.repeatSession,
      })
      assert.equal(
        repeated.assistant.content,
        chat.assistant.content,
        'deterministic Provider output must repeat for the same API turn',
      )
      report.deterministic = {
        contentEqual: true,
        firstSessionId: IDS.session,
        repeatSessionId: IDS.repeatSession,
      }
    }

    report.flow.cancel = await runCancelledSession(api, modelRoute)

    let delivery = null
    if (includeStrongFlow) {
      const deliveryCreated = await commandEventually(api, 'delivery.create', 0, {
        deliveryId: IDS.delivery,
        spec: deliverySpec(baseline),
        tasks: [],
      }, timeoutMillis)
      assertCompleted(deliveryCreated, 'delivery.create', 0)
      const deliveryAdvanced = await commandEventually(
        api,
        'delivery.advance',
        1,
        { deliveryId: IDS.delivery },
        timeoutMillis,
      )
      assertCompleted(deliveryAdvanced, 'delivery.advance', 1)
      assert.equal(deliveryAdvanced.result.deliveryId, IDS.delivery)
      delivery = await driveDelivery(api, timeoutMillis, modelRoute)
      const stageRuntime = await runtimeStageEvidence(api, delivery.detail)
      report.flow.strongflow = {
        actions: delivery.actions,
        deliveryId: IDS.delivery,
        evidenceCount: delivery.detail.evidence.length,
        providerRoute: modelRoute,
        candidateArtifact: candidateArtifactSummary(delivery.detail.currentCandidate),
        observations: delivery.observations,
        stageRuns: delivery.detail.stages.map(stage => stageRunSummary(stage, modelRoute)),
        stageRoles: delivery.detail.stages.map(stage => stage.role.toLowerCase()).toSorted(),
        stageRuntime: stageRuntime.map(item => ({
          role: item.role,
          stageRunId: item.stageRunId,
          asOfSequence: item.session.asOfSequence,
          executionJobId: item.session.executionJobId,
          workerSessionId: item.session.workerSessionId,
          codexThreadId: item.session.codexThreadId,
        })),
        status: delivery.detail.status,
        taskStatuses: delivery.detail.tasks.map(task => task.status),
        verdictStatus: delivery.detail.verdict.status,
      }
    }

    if (restart) {
      const firstDeliveryBytes = delivery === null ? null : JSON.stringify(delivery.detail)
      const firstCancelled = report.flow.cancel
      await stopServer(started.child)
      serverOutput += started.errors.join('')
      const restartProof = randomBytes(32).toString('base64url')
      proofs.push(restartProof)
      started = await startServer({
        root,
        certificate,
        directory: fixtureDirectory,
        proof: restartProof,
        checkoutRevision: baseline,
        helperExecutable,
        helperReleaseManifest,
        repositoryRoot: controlledRepository.repository,
        sourceRoot: controlledRepository.sourceRoot,
        serverEnvironment,
        serverBinary: binary,
        timeoutMillis,
      })
      report.health.afterRestart = started.health.status
      const restartedApi = new ApiClient(started.controlUrl, started.origin)
      await restartedApi.bootstrap(restartProof)
      const reloadedMessages = (await restartedApi.query(
        'session.messages.list',
        { productSessionId: IDS.session },
      )).result.items
      assert.deepEqual(reloadedMessages, chat.messages, 'Chat projection must survive Server restart')
      const reloadedCancelled = (await restartedApi.query('session.get', {
        productSessionId: IDS.cancelSession,
      })).result
      assert.equal(reloadedCancelled.state, firstCancelled.state, 'Cancellation state must survive Server restart')
      assert.equal(reloadedCancelled.revision, firstCancelled.revision, 'Cancellation revision must survive Server restart')
      let status = reloadedCancelled.state
      let deliveryBytesStable = null
      if (firstDeliveryBytes !== null) {
        const reloaded = (await restartedApi.query('delivery.get', { deliveryId: IDS.delivery })).result
        assert.equal(JSON.stringify(reloaded), firstDeliveryBytes, 'Delivery projection must survive Server restart')
        status = reloaded.status
        deliveryBytesStable = true
        assert.notEqual(delivery?.terminalCommand, null, 'terminal Delivery command must be retained for replay')
        const replayedTerminal = await restartedApi.command(
          delivery.terminalCommand.command,
          delivery.terminalCommand.expectedRevision,
          delivery.terminalCommand.payload,
          delivery.terminalCommand.requestId,
        )
        assert.equal(replayedTerminal.outcome, 'completed', 'terminal Delivery replay must complete')
        assert.equal(replayedTerminal.command, delivery.terminalCommand.command)
        assert.equal(replayedTerminal.currentRevision, delivery.detail.deliveryRevision)
        assert.equal(replayedTerminal.previousRevision, delivery.detail.deliveryRevision - 1)
      }
      report.restart = {
        deliveryBytesStable,
        messageBytesStable: true,
        status,
      }
    }
  } catch (error) {
    failure = error
  } finally {
    if (started !== null) {
      serverOutput += started.errors.join('')
      await stopServer(started.child).catch(() => {})
    }
    const combinedOutput = proofs.reduce(
      (output, secret) => redactedOutput(output, secret),
      serverOutput,
    )
    assert.equal(
      proofs.some(secret => serverOutput.includes(secret)),
      false,
      'Server output must not contain bootstrap proof',
    )
    if (failure !== null && combinedOutput.trim().length > 0) {
      const diagnostic = combinedOutput.trim().slice(-8_000)
      if (failure instanceof Error) {
        failure.message = `${failure.message}\nServer output:\n${diagnostic}`
      } else {
        failure = new Error(`${String(failure)}\nServer output:\n${diagnostic}`)
      }
    }
    removeControlledRepository({ repository: controlledRepository.repository })
    if (ownedDirectory) rmSync(fixtureDirectory, { recursive: true, force: true })
  }
  if (failure !== null) throw failure
  return report
}

function parseArguments(arguments_) {
  const options = {}
  for (let index = 0; index < arguments_.length; index += 1) {
    const argument = arguments_[index]
    if (argument === '--skip-build') {
      options.build = false
      continue
    }
    if (argument === '--no-restart') {
      options.restart = false
      continue
    }
    if (argument === '--no-repeat') {
      options.repeat = false
      continue
    }
    if (argument === '--server-binary' || argument === '--directory') {
      const value = arguments_[index + 1]
      if (value === undefined || value.startsWith('--')) fail(`${argument} requires a value`)
      options[argument === '--server-binary' ? 'serverBinary' : 'directory'] = resolve(value)
      index += 1
      continue
    }
    if (argument === '--timeout-ms') {
      const value = Number(arguments_[index + 1])
      if (!Number.isSafeInteger(value) || value <= 0) fail('--timeout-ms must be a positive integer')
      options.timeoutMillis = value
      index += 1
      continue
    }
    if (argument === '--output') {
      const value = arguments_[index + 1]
      if (value === undefined || value.startsWith('--')) fail('--output requires a value')
      options.output = resolve(value)
      index += 1
      continue
    }
    fail(`unexpected argument: ${argument}`)
  }
  return options
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const options = parseArguments(process.argv.slice(2))
    const report = await runApiProductionVertical(options)
    if (options.output !== undefined) {
      mkdirSync(dirname(options.output), { recursive: true })
      writeFileSync(options.output, `${JSON.stringify(report, null, 2)}\n`, { mode: 0o600 })
    }
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`)
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`)
    process.exitCode = 1
  }
}
