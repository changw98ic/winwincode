// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'
import Ajv2020 from 'ajv/dist/2020.js'
import addFormats from 'ajv-formats'
import { build } from 'esbuild'

const root = resolve(import.meta.dirname, '..')
const collection = JSON.parse(readFileSync(resolve(root, 'schema/winwincode/v1/schema-collection.generated.json'), 'utf8'))
const openapi = JSON.parse(readFileSync(resolve(root, 'schema/winwincode/v1/openapi.generated.json'), 'utf8'))
const ajv = new Ajv2020({ strict: false, allErrors: true })
addFormats(ajv)
ajv.addSchema(collection)
const bundle = await build({
  entryPoints: [resolve(root, 'apps/client/src/generated/control-plane-client.ts')],
  bundle: true, write: false, platform: 'node', format: 'esm',
})
const { matchesCanonicalSchema } = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`)
const schemaVersion = 'winwincode/v1'
const nodeId = `cnd_${'0'.repeat(26)}`
const requestId = 'device-bind-request-1'
const at = '2026-10-15T04:00:00.000Z'
const publicKey = Buffer.alloc(32, 7).toString('base64').replace(/=+$/u, '')
const intent = { displayName: 'Remote Device', platform: 'aarch64-apple-darwin', architecture: 'aarch64', clientVersion: '0.2.0' }
const browserResponses = new Set(['DevicePairingResponse', 'DeviceRevokeResponse', 'DeviceAuthErrorResponse', 'CandidateApplicationProjection'])

function validatorsAgree(name, value, expected) {
  const validate = ajv.compile({ $ref: `${collection.$id}#/$defs/${name}` })
  assert.equal(validate(value), expected, `${name}: ${JSON.stringify(validate.errors)}`)
  if (browserResponses.has(name)) assert.equal(matchesCanonicalSchema(name, value), expected, `generated ${name}`)
}

test('Device authentication schemas and browser validators agree on wire messages', () => {
  const messages = {
    DevicePairingRequest: { schemaVersion },
    DevicePairingResponse: { schemaVersion, pairingToken: 'pairing-token-1', expiresAt: at },
    DeviceChallengeRequest: { schemaVersion, publicKey },
    DeviceChallengeResponse: { schemaVersion, challengeId: 'challenge-1', challenge: 'nonce-1', expiresAt: at },
    DeviceBindRequest: { schemaVersion, requestId, pairingToken: 'pairing-token-1', publicKey, intent },
    DeviceBindResponse: { schemaVersion, requestId, clientNodeId: nodeId, publicClientId: '012345678901', ownerUserId: `usr_${'0'.repeat(26)}`, boundAt: at },
    DeviceRevokeRequest: { schemaVersion },
    DeviceRevokeResponse: { schemaVersion, clientNodeId: nodeId, revoked: true },
    DeviceAuthErrorResponse: { schemaVersion, code: 'DEVICE_REVOKED' },
  }
  for (const [name, value] of Object.entries(messages)) {
    validatorsAgree(name, value, true)
    validatorsAgree(name, { ...value, extra: true }, false)
    const missingVersion = { ...value }
    delete missingVersion.schemaVersion
    validatorsAgree(name, missingVersion, false)
  }
})

test('Device public keys use unpadded standard Base64 and binding retries retain a valid identity', () => {
  for (const key of [`${publicKey}=`, 'A'.repeat(42), '-'.repeat(43), null]) {
    validatorsAgree('DeviceChallengeRequest', { schemaVersion, publicKey: key }, false)
  }
  for (const badRequestId of ['', 'short', 'request with spaces']) {
    validatorsAgree('DeviceBindRequest', {
      schemaVersion, requestId: badRequestId, pairingToken: 'pairing-token-1', publicKey, intent,
    }, false)
  }
})

test('application recovery projections preserve the exact approved target in every state', () => {
  const projection = {
    applicationId: `lar_${'0'.repeat(26)}`, approvalId: 'approval-1', baseCommit: 'a'.repeat(40),
    candidateDigest: `sha256:${'b'.repeat(64)}`, targetBranch: 'feature/approved', status: 'applying',
  }
  for (const status of ['applying', 'applied', 'stale', 'awaiting_retry', 'blocked', 'failed']) {
    validatorsAgree('CandidateApplicationProjection', { ...projection, status }, true)
  }
  validatorsAgree('CandidateApplicationProjection', { ...projection, status: 'unknown' }, false)
  const missingTarget = { ...projection }
  delete missingTarget.targetBranch
  validatorsAgree('CandidateApplicationProjection', missingTarget, false)
})

test('OpenAPI distinguishes Owner cookie authorization from Device request signatures', () => {
  const paths = openapi.paths
  const pairing = paths['/api/v1/devices/pairing'].post
  const revoke = paths['/api/v1/devices/{client_node_id}/revoke'].post
  assert.deepEqual(pairing.security, [{ sessionCookie: [] }])
  assert.deepEqual(revoke.security, [{ sessionCookie: [] }])
  assert.equal(pairing.responses['201'].content['application/json'].schema.$ref, '#/components/schemas/DevicePairingResponse')
  const bind = paths['/internal/v1/device/bind'].post
  assert.deepEqual(bind.security, [{ devicePublicKey: [], deviceChallengeId: [], deviceSignature: [] }])
  for (const status of ['400', '401', '403', '409', '503']) {
    assert.equal(bind.responses[status].content['application/json'].schema.$ref, '#/components/schemas/DeviceAuthErrorResponse')
  }
})
